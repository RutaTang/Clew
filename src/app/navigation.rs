//! Getting around the project: go-to-definition and references with their
//! location lists, full-text search, the fuzzy finder, and resolving a call
//! site to its line.
//!
//! Its messages, [`NavMsg`], arrive through `App::update_nav`.

use crate::app::prelude::*;
use crate::*;

/// Read the preview line of each `(hit index, path, 0-based line)` from disk,
/// one read per file, keeping only the lines asked for: the file itself is
/// dropped before the next is read (a memo of every file's lines held up to
/// every referenced file, 4 MiB each, at once). Blocking.
///
/// Leaf guard + cap, like every other client-side source read: the paths come
/// from the language server, so a leaf may be a symlink or a FIFO.
/// `read_capped` is `open_plain` (`O_NOFOLLOW | O_NONBLOCK`, regular-file check
/// on the open handle) plus the cap. Deliberately NOT `read_confined_capped`:
/// references legitimately land in dependency and stdlib sources outside the
/// root (the same `external_local` allowance `open_file` makes), and
/// containment would blank those previews — it is path-level, and an in-root
/// symlink is already refused by `O_NOFOLLOW`. The VIEWER's cap, not the
/// index's: a file the pane will open must be one the preview can read, and it
/// is the number the server's own reference preview uses (`agent_lsp.rs`).
pub(crate) fn read_location_previews(
    targets: Vec<(usize, PathBuf, usize)>,
) -> Vec<(usize, String)> {
    // Per file, the (line, hit index) pairs wanted from it, in line order.
    let mut by_file: HashMap<PathBuf, Vec<(usize, usize)>> = HashMap::new();
    for (i, path, line) in targets {
        by_file.entry(path).or_default().push((line, i));
    }
    let mut out = Vec::new();
    // One file's text at a time, dropped once its lines are taken: a memo of
    // every referenced file held them all at once (up to two thousand files
    // of up to 4 MiB).
    for (path, mut wanted) in by_file {
        let Some(text) = clew_core::statefile::read_capped(&path, viewer::MAX_FILE_BYTES as u64)
            .map(PreviewText::new)
        else {
            continue;
        };
        wanted.sort_unstable();
        // One pass over the file, stopping after the last line asked for.
        let mut next = wanted.iter().peekable();
        for (n, text_line) in text.lines().enumerate() {
            let Some(&&(line, _)) = next.peek() else {
                break;
            };
            if line > n {
                continue;
            }
            while let Some(&(_, i)) = next.next_if(|(line, _)| *line == n) {
                out.push((i, text_line.trim().to_string()));
            }
        }
    }
    out.sort_unstable_by_key(|(i, _)| *i);
    out
}

/// One file's text while [`read_location_previews`] takes its lines. In tests
/// it is counted while alive (`preview_texts`): how many were held at once.
struct PreviewText(String);

impl PreviewText {
    fn new(text: String) -> PreviewText {
        #[cfg(test)]
        preview_texts::held(1);
        PreviewText(text)
    }
}

impl std::ops::Deref for PreviewText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl Drop for PreviewText {
    fn drop(&mut self) {
        preview_texts::held(-1);
    }
}

/// The count behind [`PreviewText`], per thread: the texts alive now, and
/// the most alive at once since [`preview_texts::reset`].
#[cfg(test)]
pub(crate) mod preview_texts {
    use std::cell::Cell;
    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }
    pub(super) fn held(delta: isize) {
        let live = LIVE.get() + delta;
        LIVE.set(live);
        PEAK.set(PEAK.get().max(live));
    }
    pub(crate) fn reset() {
        PEAK.set(LIVE.get());
    }
    pub(crate) fn peak() -> isize {
        PEAK.get()
    }
}

/// The status line for a finished search: the match count (capped or not),
/// and — when files could not be searched — how many, naming the first.
pub(crate) fn search_status(
    hits: usize,
    skipped: &[search::SkippedFile],
    skipped_total: usize,
) -> String {
    let matches = if hits >= search::MAX_HITS {
        format!("{hits}+ matches (capped)")
    } else {
        format!("{hits} matches")
    };
    match skipped.first() {
        None => matches,
        Some(first) => {
            let n = skipped_total.max(skipped.len());
            let files = if n == 1 { "file" } else { "files" };
            format!(
                "{matches} · {n} {files} not searched ({}: {})",
                first.rel, first.reason
            )
        }
    }
}

impl App {
    /// Run a navigation request from the active pane's cursor.
    pub(crate) fn goto_at_cursor(&mut self, kind: GotoKind) -> Task<Message> {
        let pane = self.proj.active;
        let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
            return Task::none();
        };
        self.goto_request(pane, line, col, kind)
    }

    /// Dispatch an LSP navigation request (definition / references / …) at a
    /// clicked or cursor position.
    pub(crate) fn goto_request(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        kind: GotoKind,
    ) -> Task<Message> {
        // Pull everything we need from the viewer before mutating self.
        let Some((lang, path, source_line)) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| {
                v.lang_key.map(|l| {
                    (
                        l,
                        v.abs.clone(),
                        v.source_line(line).unwrap_or("").to_string(),
                    )
                })
            })
        else {
            return Task::none();
        };

        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready (⌘T to search symbols)");
                return Task::none();
            }
        };
        // The display column, in the server's own position encoding.
        let character = viewer::Col(col).to_offset(&source_line, client.encoding);
        self.status = format!("{}…", kind.verb());
        let is_references = matches!(kind, GotoKind::References);
        // Mint this request's identity: references paint the Search sidebar
        // (so they share its counter); definitions jump the editor.
        let seq = if is_references {
            self.proj.search_seq += 1;
            // A server-side text search still in flight is superseded by this
            // request exactly as a newer submission would supersede it: it is
            // about to lose the sidebar to the reference list. `search_seq`
            // alone does not retire it — the server path is guarded by the
            // request id in `pending_search`, not by the counter (only the
            // in-process fallback's `SearchDone` carries a seq) — so its
            // `SearchResults` (or a correlated `Error`) would land on top of
            // the references, showing grep hits under the "(references)"
            // label. Clearing the spinner with it is not optional: an empty or
            // failed references reply never reaches `show_references`, and the
            // dropped search reply can no longer clear it either.
            self.proj.link.pending_search = None;
            self.proj.search.running = false;
            self.proj.search_seq
        } else {
            self.proj.goto_seq += 1;
            self.proj.goto_seq
        };
        let stamp = self.stamp();
        Task::perform(
            async move { client.navigate(kind.method(), &path, line, character).await },
            move |result| {
                let stamp = stamp.clone();
                if is_references {
                    Message::Nav(NavMsg::ReferencesResult { stamp, seq, result })
                } else {
                    Message::Nav(NavMsg::DefinitionResult { stamp, seq, result })
                }
            },
        )
    }

    /// Resolve the definition at a clicked (line, display col) in `pane`.
    pub(crate) fn goto_definition(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        self.goto_request(pane, line, col, GotoKind::Definition)
    }

    pub(crate) fn on_definition_result(
        &mut self,
        result: Result<Vec<lsp::client::Target>, String>,
    ) -> Task<Message> {
        match result {
            // Several definitions (a trait method's impls, overloads, a
            // symbol defined per platform): jumping to the first one silently
            // hid the rest. List them all in the Search sidebar to pick from,
            // the way references are shown.
            Ok(targets) if targets.len() > 1 => {
                self.status = format!(
                    "{} definitions — pick one in the Search list",
                    targets.len()
                );
                self.show_locations("(definitions)", targets)
            }
            Ok(targets) if !targets.is_empty() => {
                let t = &targets[0];
                let abs = t.path.clone();
                let target_line = t.line + 1;
                // Clear the "Looking up definition…" progress; the jump itself
                // is the feedback (otherwise the status stays stuck on it).
                self.status.clear();
                self.open_file(abs, Some(target_line), true)
            }
            Ok(_) => {
                self.status = "No definition found".into();
                Task::none()
            }
            Err(e) => {
                self.status = format!("Definition failed: {e}");
                Task::none()
            }
        }
    }

    pub(crate) fn on_references_result(
        &mut self,
        result: Result<Vec<lsp::client::Target>, String>,
    ) -> Task<Message> {
        match result {
            Ok(refs) if !refs.is_empty() => {
                self.status = format!("{} reference(s) — showing them in Search", refs.len());
                self.show_locations("(references)", refs)
            }
            Ok(_) => {
                self.status = "No references".into();
                Task::none()
            }
            Err(e) => {
                self.status = format!("References failed: {e}");
                Task::none()
            }
        }
    }

    /// List LSP locations (references, or the several targets of one
    /// definition) in the Search sidebar under `label`.
    ///
    /// Each hit's preview is its line of source. A file open in a pane gives
    /// it from memory; any other is read from disk — on the blocking pool,
    /// each file once however many hits it holds, arriving as
    /// `LocationPreviews`. It used to be read right here, up to two thousand
    /// files of up to 4 MiB, on the thread that serves every window.
    pub(crate) fn show_locations(
        &mut self,
        label: &str,
        targets: Vec<lsp::client::Target>,
    ) -> Task<Message> {
        // Painting the sidebar supersedes whatever it was waiting for — a
        // server-side text search included (see `goto_request`).
        self.proj.search_seq += 1;
        self.proj.link.pending_search = None;
        let seq = self.proj.search_seq;
        let mut to_read: Vec<(usize, PathBuf, usize)> = Vec::new();
        let hits: Vec<SearchHit> = targets
            .into_iter()
            .take(search::MAX_HITS)
            .enumerate()
            .map(|(i, t)| {
                let rel = self.rel_of(&t.path);
                // The pane's lines are indexed: one lookup per hit, where
                // walking the source to the line cost the file's length per
                // hit, on this thread.
                let from_pane = self
                    .proj
                    .panes
                    .iter()
                    .flatten()
                    .find(|v| v.abs == t.path)
                    .filter(|v| t.line < v.lines.len())
                    .map(|v| v.line_text(t.line + 1).trim().to_string());
                if from_pane.is_none() {
                    to_read.push((i, t.path.clone(), t.line));
                }
                SearchHit {
                    abs: t.path,
                    rel,
                    line: t.line + 1,
                    preview: from_pane.unwrap_or_default(),
                }
            })
            .collect();
        self.proj.search.query = label.to_string();
        self.proj.search.ran = true;
        self.proj.search.running = false;
        self.proj.search.error = None;
        // A location list is not a search: nothing was skipped.
        self.proj.search.skipped.clear();
        self.proj.search.hits = hits;
        self.sidebar = SidebarTab::Search;
        self.code_focused = false;
        // The preview comes from the file itself, so it may only be read where
        // the file lives. For a remote project an open pane's text is the only
        // local source of truth; this machine's disk at the remote's path is a
        // different machine's code, and showing ITS line next to a remote hit
        // is worse than showing none.
        if to_read.is_empty() || !self.local_project_state() {
            return Task::none();
        }
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || read_location_previews(to_read))
                    .await
                    .unwrap_or_default()
            },
            move |previews| {
                Message::Nav(NavMsg::LocationPreviews {
                    stamp: stamp.clone(),
                    seq,
                    previews,
                })
            },
        )
    }

    /// Kick off a project search from the current query and options.
    pub(crate) fn run_search(&mut self) -> Task<Message> {
        let Some((root, files)) = self
            .proj
            .project
            .as_ref()
            .map(|p| (p.root.clone(), p.files.clone()))
        else {
            return Task::none();
        };
        if self.proj.search.query.trim().is_empty() {
            self.proj.search.hits.clear();
            self.proj.search.error = None;
            self.proj.search.ran = false;
            return Task::none();
        }
        self.proj.search.running = true;
        self.proj.search.ran = true;
        self.proj.search.hits.clear();
        // This submission supersedes any earlier one still in flight.
        self.proj.search_seq += 1;
        let seq = self.proj.search_seq;
        let opts = search::SearchOptions {
            query: self.proj.search.query.trim().to_string(),
            regex: self.proj.search.regex,
            case_sensitive: self.proj.search.case_sensitive,
            whole_word: self.proj.search.whole_word,
            include: self.proj.search.include.clone(),
            exclude: self.proj.search.exclude.clone(),
        };

        // Preferred path: run the search on the clew-server over the protocol.
        // Results come back as `Event::SearchResults` (see `handle_server_event`).
        if self.server.is_up() {
            let request = clew_protocol::Request::Search {
                query: opts.query.clone(),
                regex: opts.regex,
                case_sensitive: opts.case_sensitive,
                whole_word: opts.whole_word,
                include: opts.include.clone(),
                exclude: opts.exclude.clone(),
            };
            // Re-sent (bounded) should the server refuse it while its scan
            // of a just-opened project runs (see `send_retrying`).
            if let Some(id) = self.send_retrying(request) {
                // The reply is applied only while this is still the latest
                // in-flight search (see the SearchResults reply arm).
                self.proj.link.pending_search = Some(id);
                return Task::none();
            }
        }

        // Remote project with the transport down: fail closed rather than
        // grep the client's own filesystem — the file list's absolute paths
        // belong to the remote host, and a same-pathed local tree would be a
        // different project's contents.
        if self.connection.is_remote() {
            self.proj.search.running = false;
            self.proj.search.error = Some(
                "Disconnected from the remote host — search will work again once reconnected"
                    .into(),
            );
            return Task::none();
        }
        // Fallback: server not connected yet (or its channel closed) — run the
        // same search in-process so search never depends on handshake timing.
        // Confined to the project root: every listed file is re-checked to be
        // inside it before it is read.
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || search::search_report(&root, files, opts))
                    .await
                    .unwrap_or_else(|_| search::SearchReport {
                        error: Some("the search failed unexpectedly".into()),
                        ..Default::default()
                    })
            },
            move |result| {
                Message::Nav(NavMsg::SearchDone {
                    stamp: stamp.clone(),
                    seq,
                    result,
                })
            },
        )
    }

    /// Apply a completed search to the UI — shared by the server path and the
    /// in-process fallback so both render results identically. `skipped_total`
    /// counts every file that could not be searched (`report.skipped` may list
    /// only a bounded few of them).
    pub(crate) fn apply_search_result(
        &mut self,
        report: search::SearchReport,
        skipped_total: usize,
    ) {
        self.proj.search.running = false;
        self.proj.search.error = report.error.clone();
        self.status = match &report.error {
            Some(e) => e.clone(),
            None => search_status(report.hits.len(), &report.skipped, skipped_total),
        };
        self.proj.search.hits = report.hits;
        self.proj.search.skipped = report.skipped;
    }

    pub(crate) fn refresh_finder(&mut self) {
        match self.proj.finder.mode {
            FinderMode::Files => {
                if let Some(p) = &self.proj.project {
                    let files = p.files.clone();
                    self.proj.finder.refresh_files(&files);
                }
            }
            FinderMode::Symbols => {
                let symbols = self.fresh_symbol_index();
                self.proj.finder.refresh_symbols(&symbols);
            }
        }
    }

    pub(crate) fn on_finder_opened(&mut self, mode: FinderMode) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        self.proj.finder.open = true;
        self.proj.finder.mode = mode;
        self.proj.finder.query.clear();
        self.code_focused = false; // the finder input takes focus
        self.refresh_finder();
        operation::focus(ui::finder_input_id())
    }

    pub(crate) fn on_finder_confirm(&mut self) -> Task<Message> {
        if let Some(line) = self.proj.finder.goto_line() {
            self.proj.finder.open = false;
            if let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) {
                return self.open_file(abs, Some(line), true);
            }
            return Task::none();
        }
        match self
            .proj
            .finder
            .results
            .get(self.proj.finder.selected)
            .copied()
        {
            Some(idx) => self.finder_open_index(idx),
            None => Task::none(),
        }
    }

    pub(crate) fn on_goto_line_requested(&mut self) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        self.proj.finder.open = true;
        self.proj.finder.mode = FinderMode::Files;
        self.proj.finder.query = ":".to_string();
        self.refresh_finder();
        Task::batch([
            operation::focus(ui::finder_input_id()),
            operation::move_cursor_to_end(ui::finder_input_id()),
        ])
    }

    pub(crate) fn finder_open_index(&mut self, idx: usize) -> Task<Message> {
        self.proj.finder.open = false;
        match self.proj.finder.mode {
            FinderMode::Files => {
                let Some(entry) = self
                    .proj
                    .project
                    .as_ref()
                    .and_then(|p| p.files.get(idx))
                    .cloned()
                else {
                    return Task::none();
                };
                self.open_file(entry.abs, None, true)
            }
            FinderMode::Symbols => {
                let Some(entry) = self.proj.symbol_index.get(idx).cloned() else {
                    return Task::none();
                };
                self.open_file(entry.abs, Some(entry.line), true)
            }
        }
    }

    /// The function/method defined exactly at `(file, line1)`, if any — recorded
    /// with a history entry so it can be re-anchored across edits.
    pub(crate) fn symbol_name_at(&self, file: &Path, line1: usize) -> Option<String> {
        self.proj
            .symbol_index_by_file
            .get(file)?
            .iter()
            .find_map(|s| {
                (s.line == line1 && matches!(s.kind.as_str(), "function" | "method"))
                    .then(|| s.name.clone())
            })
    }

    /// Whether `(file, name)` is a test function, per the symbol index.
    pub fn is_test_symbol(&self, file: &Path, name: &str) -> bool {
        self.proj
            .symbol_index_by_file
            .get(file)
            .is_some_and(|syms| syms.iter().any(|s| s.name == name && s.is_test))
    }

    /// The entry-point kind of `(file, name)`, per the symbol index: `Some`
    /// for a main, a route, a command or a handler (see
    /// [`index::entry_kind`]), `None` for every other function.
    pub fn entry_kind_of(&self, file: &Path, name: &str) -> Option<index::EntryKind> {
        self.proj.symbol_index_by_file.get(file).and_then(|syms| {
            syms.iter()
                .find_map(|s| (s.name == name).then_some(s.entry)?)
        })
    }

    /// The class an entry point belongs to for a "reached from" walk
    /// ([`projectcalls::ProjectCallGraph::paths_from_entries`]): a main,
    /// route, command or handler ranks first, a test second, and every other
    /// function is not a place execution enters.
    pub fn entry_class_of(&self, file: &Path, name: &str) -> Option<u8> {
        let syms = self.proj.symbol_index_by_file.get(file)?;
        let sym = syms.iter().find(|s| s.name == name)?;
        if sym.entry.is_some() {
            Some(0)
        } else if sym.is_test {
            Some(1)
        } else {
            None
        }
    }

    /// Whether the index knows any entry point at all — a `main`, a route, a
    /// command, a handler or a test. A project with none (a library) has no
    /// "reached from" chains to show, and is not told so function by function.
    pub fn has_entry_points(&self) -> bool {
        self.proj
            .symbol_index_by_file
            .values()
            .any(|syms| syms.iter().any(|s| s.entry.is_some() || s.is_test))
    }

    /// The first 1-based line where `caller` (in `caller_file`) calls `callee`,
    /// found by re-parsing the caller's live source (the open pane, else disk).
    pub(crate) fn call_site_line(
        &self,
        caller_file: &Path,
        caller: &str,
        callee: &str,
    ) -> Option<usize> {
        let lang = crate::highlight::detect(caller_file)?;
        // An open pane's text is the file wherever it lives. Falling back to
        // this machine's disk is only meaningful for a LOCAL project: for a
        // remote one the path names another host, and a same-pathed local
        // file would silently place the call in the wrong code.
        let source = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == caller_file)
            .map(|v| v.source.as_ref().clone())
            .or_else(|| {
                // Same guard as every other client-side project-source read
                // (the symbol index, `tasks::gather_*`): the
                // call-graph node holding this path came from a scan that can
                // be minutes old, so the leaf may since have become a symlink
                // pointing outside the project — whose text would be parsed as
                // this project's code — or a FIFO. This runs on the iced update
                // loop (`NavMsg::JumpToCall`), which serves EVERY window, so a
                // blocking `open(2)` here freezes the whole interface rather
                // than one background task. Containment costs nothing: call
                // graph nodes are in-root by construction.
                self.proj
                    .project
                    .as_ref()
                    .filter(|_| self.local_project_state())
                    .and_then(|p| {
                        clew_core::fs_scan::read_confined_capped(
                            &p.root,
                            caller_file,
                            index::MAX_INDEX_FILE_BYTES,
                        )
                    })
            })?;
        projectcalls::calls_of(&source, lang)
            .into_iter()
            .filter(|cs| cs.callee == callee && cs.caller.as_deref() == Some(caller))
            .map(|cs| cs.line)
            .min()
    }
}

impl App {
    /// Handle a [`NavMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_nav(&mut self, message: NavMsg) -> Task<Message> {
        match message {
            NavMsg::SearchQueryChanged(query) => {
                self.proj.search.query = query;
                Task::none()
            }
            NavMsg::SearchToggle(opt) => {
                match opt {
                    SearchOpt::Regex => self.proj.search.regex = !self.proj.search.regex,
                    SearchOpt::Case => {
                        self.proj.search.case_sensitive = !self.proj.search.case_sensitive
                    }
                    SearchOpt::WholeWord => {
                        self.proj.search.whole_word = !self.proj.search.whole_word
                    }
                }
                // Re-run live so the effect of the toggle is immediate.
                self.run_search()
            }
            NavMsg::SearchIncludeChanged(s) => {
                self.proj.search.include = s;
                Task::none()
            }
            NavMsg::SearchExcludeChanged(s) => {
                self.proj.search.exclude = s;
                Task::none()
            }
            NavMsg::SearchSubmitted => self.run_search(),
            NavMsg::SearchDone { seq, result, .. } => {
                // Only the latest submission may paint the Search sidebar.
                if seq == self.proj.search_seq {
                    let skipped_total = result.skipped.len();
                    self.apply_search_result(result, skipped_total);
                }
                Task::none()
            }
            NavMsg::FinderOpened(mode) => self.on_finder_opened(mode),
            NavMsg::FinderClosed => {
                self.proj.finder.open = false;
                self.code_focused = true; // back to reading
                Task::none()
            }
            NavMsg::FinderQueryChanged(query) => {
                self.proj.finder.query = query;
                self.refresh_finder();
                Task::none()
            }
            NavMsg::FinderPick { abs, line } => {
                self.proj.finder.open = false;
                self.open_file(abs, line, true)
            }
            NavMsg::FinderConfirm => self.on_finder_confirm(),
            NavMsg::GotoLineRequested => self.on_goto_line_requested(),
            NavMsg::DefinitionResult { seq, result, .. } => {
                // A superseded request (a newer gd, a project switch bumping
                // the counter) must not jump the editor.
                if seq != self.proj.goto_seq {
                    return Task::none();
                }
                self.on_definition_result(result)
            }
            NavMsg::ReferencesResult { seq, result, .. } => {
                if seq != self.proj.search_seq {
                    return Task::none();
                }
                self.on_references_result(result)
            }
            NavMsg::LocationPreviews { seq, previews, .. } => {
                // Only the list still on screen takes these previews: a newer
                // search or reference list has replaced the hits they index.
                if seq == self.proj.search_seq {
                    for (i, preview) in previews {
                        if let Some(hit) = self.proj.search.hits.get_mut(i) {
                            hit.preview = preview;
                        }
                    }
                    self.proj.search.running = false;
                }
                Task::none()
            }
            NavMsg::JumpToCall {
                caller_file,
                caller,
                callee,
            } => {
                let line = self
                    .call_site_line(&caller_file, &caller, &callee)
                    .or_else(|| {
                        self.proj
                            .symbol_index_by_file
                            .get(&caller_file)
                            .and_then(|syms| syms.iter().find(|s| s.name == caller).map(|s| s.line))
                    });
                self.open_file(caller_file, line, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `call_site_line` falls back to this machine's disk when the caller is
    /// not open in a pane, and the path it reads comes from a call graph built
    /// off a scan that can be minutes old. Both refusals matter, and neither
    /// existed before: a leaf swapped for a symlink out of the project put
    /// another file's call sites under this project's name, and an over-cap
    /// file was pulled whole into the iced update loop that serves EVERY
    /// window (a FIFO there froze the entire interface).
    #[test]
    #[cfg(unix)]
    fn call_site_line_refuses_a_symlink_out_of_the_project_and_an_over_cap_file() {
        use crate::app::tests::{blank_app, test_dir};
        const SRC: &str = "fn caller() {\n    callee();\n}\n";
        let dir = test_dir("callsite-confine");
        let outside_dir = test_dir("callsite-confine-outside");
        for d in [&dir, &outside_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        // Outside the project, holding a call `caller -> callee` that must
        // never be reported as this project's.
        let outside = outside_dir.join("outside.rs");
        std::fs::write(&outside, SRC).unwrap();
        let link = dir.join("src/linked.rs");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        // A plain in-project caller (the control) and one past the cap.
        let plain = dir.join("src/plain.rs");
        std::fs::write(&plain, SRC).unwrap();
        let big = dir.join("src/big.rs");
        let padding = "// ".to_string() + &"x".repeat(index::MAX_INDEX_FILE_BYTES as usize) + "\n";
        std::fs::write(&big, format!("{padding}{SRC}")).unwrap();

        // `blank_app` reads `trust.toml` / `connections.toml` from the suite's
        // isolated data directory, never the developer's real clew data.
        let mut app = blank_app();
        // A CLEW_SSH in the developer's environment would otherwise make
        // `local_project_state()` false and pass this test vacuously.
        app.connection = connect::ConnTarget::Local;
        app.proj.project = Some(Project {
            root: dir.clone(),
            tree: DirNode::default(),
            files: std::sync::Arc::new(Vec::new()),
            truncated: false,
        });

        assert_eq!(
            app.call_site_line(&plain, "caller", "callee"),
            Some(2),
            "an ordinary in-project caller must still resolve"
        );
        assert_eq!(
            app.call_site_line(&link, "caller", "callee"),
            None,
            "a symlink pointing out of the project was followed"
        );
        assert_eq!(
            app.call_site_line(&big, "caller", "callee"),
            None,
            "a file past the index cap was read whole on the update loop"
        );

        for d in [&dir, &outside_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[cfg(test)]
mod location_tests {
    use super::*;

    /// The reference preview is read off this machine's disk at whatever path
    /// the language server named, on the iced update loop that serves EVERY
    /// window. Before the guard it was a bare `read_to_string`: a symlinked or
    /// over-cap leaf was read whole there, and a FIFO froze the interface.
    ///
    /// Containment is deliberately NOT part of that guard — references
    /// legitimately land in dependency and stdlib sources outside the root
    /// (the `external_local` allowance `open_file` makes, and the same choice
    /// the server's own reference preview makes in `agent_lsp.rs`) — so this
    /// pins the external preview as WORKING alongside the two refusals.
    #[test]
    #[cfg(unix)]
    fn reference_preview_refuses_a_symlink_and_an_over_cap_file_but_keeps_external_sources() {
        use crate::app::tests::{blank_app, run_task, test_dir};
        let dir = test_dir("refpreview-guard");
        let outside_dir = test_dir("refpreview-guard-outside");
        for d in [&dir, &outside_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let outside = outside_dir.join("dep.rs");
        std::fs::write(&outside, "    let dep = 1;\n").unwrap();
        let link = dir.join("src/linked.rs");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let plain = dir.join("src/plain.rs");
        std::fs::write(&plain, "    let here = 1;\n").unwrap();
        let big = dir.join("src/big.rs");
        std::fs::write(
            &big,
            format!(
                "// {}\n    let big = 1;\n",
                "x".repeat(viewer::MAX_FILE_BYTES)
            ),
        )
        .unwrap();

        // `blank_app` runs against the suite's isolated data directory, and
        // the transport is always local in tests (`CLEW_SSH` is ignored).
        let mut app = blank_app();
        app.proj.project = Some(Project {
            root: dir.clone(),
            tree: DirNode::default(),
            files: std::sync::Arc::new(Vec::new()),
            truncated: false,
        });

        let target = |p: &std::path::Path, line: usize| lsp::client::Target {
            path: p.to_path_buf(),
            line,
            character: 0,
        };
        let read = app.show_locations(
            "(references)",
            vec![
                target(&plain, 0),
                target(&link, 0),
                target(&big, 1),
                target(&outside, 0),
            ],
        );
        // Nothing was read on the update loop: the list is on screen at once,
        // and the previews arrive from the blocking pool.
        assert!(app.proj.search.hits.iter().all(|h| h.preview.is_empty()));
        for msg in run_task(read) {
            let _ = app.update(msg);
        }
        let previews: Vec<&str> = app
            .proj
            .search
            .hits
            .iter()
            .map(|h| h.preview.as_str())
            .collect();
        assert_eq!(
            previews,
            vec!["let here = 1;", "", "", "let dep = 1;"],
            "expected: in-project preview, symlink refused, over-cap refused, \
             external dependency source still previewed"
        );

        // Previews for a list the sidebar has since replaced are dropped.
        let stale = app.show_locations("(references)", vec![target(&plain, 0)]);
        app.proj.search_seq += 1; // a newer search took the sidebar
        app.proj.search.hits[0].preview = String::new();
        for msg in run_task(stale) {
            let _ = app.update(msg);
        }
        assert_eq!(
            app.proj.search.hits[0].preview, "",
            "a stale preview landed"
        );

        for d in [&dir, &outside_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
