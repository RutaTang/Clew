//! Getting around the project: go-to-definition and references with their
//! location lists, full-text search, the fuzzy finder, and resolving a call
//! site to its line.
//!
//! Its messages, [`NavMsg`], arrive through `App::update_nav`.

use crate::app::keys;
use crate::app::message::TreeKey;
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
    targets: Vec<(usize, PathBuf, usize, usize)>,
    enc: viewer::PositionEncoding,
) -> Vec<(usize, String)> {
    let at: HashMap<usize, usize> = targets.iter().map(|&(i, _, _, ch)| (i, ch)).collect();
    let lines = targets
        .into_iter()
        .map(|(i, path, line, _)| (i, path, line))
        .collect();
    read_location_lines(lines)
        .into_iter()
        .map(|(i, line)| {
            let preview = location_preview(&line, at.get(&i).copied(), enc);
            (i, preview)
        })
        .collect()
}

/// A result's preview in its language's colours, as `(text, style)` spans:
/// the line highlighted on its own (its file is not read for it), and an
/// ellipsis the preview was cut with left uncoloured, as no part of the
/// code. Plain, as one span, where the language is unknown.
pub(crate) fn preview_spans(
    preview: &str,
    lang: Option<&'static str>,
) -> Vec<(String, Option<u8>)> {
    let shown = highlight::line_in_order(preview);
    let (lead, rest) = match shown.strip_prefix('…') {
        Some(rest) => ("…", rest),
        None => ("", &*shown),
    };
    let (code, tail) = match rest.strip_suffix('…') {
        Some(code) => (code, "…"),
        None => (rest, ""),
    };
    let mut out: Vec<(String, Option<u8>)> = Vec::new();
    if !lead.is_empty() {
        out.push((lead.to_string(), None));
    }
    match highlight::highlight_lines(code, lang).into_iter().next() {
        Some(line) => out.extend(line.spans),
        None if !code.is_empty() => out.push((code.to_string(), None)),
        None => {}
    }
    if !tail.is_empty() {
        out.push((tail.to_string(), None));
    }
    // Drawn only as the text it is: a span set the highlighter changed
    // (a tab widened, a control replaced) is left for the plain preview.
    let same = out.iter().map(|(t, _)| t.as_str()).collect::<String>() == *shown;
    if same {
        out
    } else {
        vec![(shown.into_owned(), None)]
    }
}

/// The identifier around byte `at` of `line` (`at` on it, or just past
/// it): what a definitions or references request was asked about.
pub(crate) fn word_at_byte(line: &str, at: usize) -> Option<String> {
    let word = |c: char| c == '_' || c.is_alphanumeric();
    let at = at.min(line.len());
    if !line.is_char_boundary(at) {
        return None;
    }
    let start = line[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| word(c))
        .last()
        .map_or(at, |(i, _)| i);
    let end = line[at..]
        .char_indices()
        .find(|&(_, c)| !word(c))
        .map_or(line.len(), |(i, _)| at + i);
    (end > start).then(|| line[start..end].to_string())
}

/// A location's line as the Search list shows it: as a search hit's is, from
/// just before the place it points at, so a hit far along a long line is not
/// cut off past the list's edge. `character` is in the server's `enc` units.
pub(crate) fn location_preview(
    line: &str,
    character: Option<usize>,
    enc: viewer::PositionEncoding,
) -> String {
    let at = character.map(|ch| {
        let mut units = 0;
        line.char_indices()
            .find(|&(_, c)| {
                let past = units >= ch;
                units += enc.units(c);
                past
            })
            .map_or(line.len(), |(i, _)| i)
    });
    clew_core::search::preview_around(line, at)
        .trim_end()
        .to_string()
}

/// A hit's preview as given out to be coloured and marked
/// ([`App::style_search_previews`]).
pub(crate) struct PreviewToStyle {
    lang: Option<&'static str>,
    preview: String,
    /// Its whole line, as an open pane has it.
    line: Option<String>,
    abs: PathBuf,
    line0: usize,
}

/// Each preview's colours and marks, by hit ([`style_previews`]).
type StyledPreviews = (
    Vec<Vec<(String, Option<u8>)>>,
    Vec<Option<Vec<(usize, usize)>>>,
);

/// How many of a search's results have their whole line read for their
/// marks ([`style_previews`]).
const LINES_READ_FOR_MARKS: usize = 500;

/// Each preview in its language's colours, and where it marks what
/// `matcher` matches — found in its whole line, read from its file in a
/// local project when no pane had it.
fn style_previews(
    hits: Vec<PreviewToStyle>,
    matcher: Option<&clew_core::search::PreviewMatcher>,
    local: bool,
) -> StyledPreviews {
    let mut lines: Vec<Option<String>> = hits.iter().map(|h| h.line.clone()).collect();
    if local && matcher.is_some() {
        // Read only where marks can come of it — a preview there is (a
        // location list's are read after this pass) — and for the first
        // results only: a search of thousands waited on every one of their
        // files before any colour showed. The rest are marked in their
        // previews, as the sidebar does without a line.
        let unread: Vec<(usize, PathBuf, usize)> = hits
            .iter()
            .enumerate()
            .take(LINES_READ_FOR_MARKS)
            .filter(|(_, h)| h.line.is_none() && !h.preview.trim().is_empty())
            .map(|(i, h)| (i, h.abs.clone(), h.line0))
            .collect();
        for (i, line) in read_location_lines(unread) {
            lines[i] = Some(line);
        }
    }
    let styled = hits
        .iter()
        .map(|h| preview_spans(&h.preview, h.lang))
        .collect();
    let marks = hits
        .iter()
        .zip(&lines)
        .map(|(h, line)| marks_in_line(matcher?, line.as_deref()?, &h.preview))
        .collect();
    (styled, marks)
}

/// Where `preview`, cut from `line`, marks what `matcher` matches in the
/// whole line: found in the line, so a pattern anchored at its start or
/// matching its tabs, a match the cut ends, a whole word the cut's `…`
/// would make of a longer one, are marked as the search matched them.
/// `None` when the preview is not this line's (changed since it was cut)
/// or cannot be placed in it.
pub(crate) fn marks_in_line(
    matcher: &clew_core::search::PreviewMatcher,
    line: &str,
    preview: &str,
) -> Option<Vec<(usize, usize)>> {
    use clew_core::search::{preview_around, preview_marks};
    let ranges = matcher.ranges(line);
    // Cut where the search cut it: at its first match, or a reference at
    // its own name.
    let at = std::iter::once(None)
        .chain(ranges.iter().map(|r| Some(r.0)))
        .find(|&at| preview_around(line, at).trim_end() == preview.trim_end())?;
    preview_marks(line, at, &ranges)
}

/// [`read_location_previews`], each line as the file has it: untrimmed, so a
/// column the language server gives for it lands where it points (a trimmed
/// line shifts every column by the indentation). A trailing `\r` is dropped.
pub(crate) fn read_location_lines(targets: Vec<(usize, PathBuf, usize)>) -> Vec<(usize, String)> {
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
                out.push((i, text_line.trim_end_matches('\r').to_string()));
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

/// "1 match", "2 matches".
pub(crate) fn count_matches(n: usize) -> String {
    trn!("1 match", "{n} matches", n)
}

/// The status line for a finished search: the match count (capped or not),
/// and — when files could not be searched — how many, naming the first.
pub(crate) fn search_status(
    hits: usize,
    skipped: &[search::SkippedFile],
    skipped_total: usize,
) -> String {
    let matches = if hits >= search::MAX_HITS {
        tr!("{count}+ matches (capped)", count = hits)
    } else {
        count_matches(hits)
    };
    match skipped.first() {
        None => matches,
        Some(first) => {
            let n = skipped_total.max(skipped.len());
            trn!(
                "{matches}; 1 file not searched ({file}: {reason})",
                "{matches}; {n} files not searched ({file}: {reason})",
                n,
                matches = matches,
                file = first.rel,
                reason = crate::i18n::lookup(&first.reason)
            )
        }
    }
}

/// How long typing in the search sidebar must pause before it searches.
pub(crate) const LIVE_SEARCH_PAUSE: std::time::Duration = std::time::Duration::from_millis(300);

/// The kinds of symbol a reading-trail stop is named for, and re-anchored
/// to as edits move it: functions, and the types a jump lands on (Go to
/// Type Definition). A class read as `cart.py:6` in the trail.
pub(crate) fn names_trail_stops(kind: &str) -> bool {
    matches!(
        kind,
        "function" | "method" | "class" | "struct" | "enum" | "trait" | "interface" | "type"
    )
}

/// How many of the files read lately lead the file finder with nothing typed.
const RECENT_FILES: usize = 20;
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
                self.status = tr!(
                    "No {language} server ready",
                    language = crate::app::languages::language_name(lang)
                );
                return Task::none();
            }
        };
        // The display column, in the server's own position encoding.
        let character = viewer::Col(col).to_offset(&source_line, client.encoding);
        let word = word_at_byte(
            &source_line,
            viewer::Col(col).to_offset(&source_line, viewer::PositionEncoding::Utf8),
        );
        self.status = format!("{}…", kind.verb());
        let is_references = matches!(kind, GotoKind::References);
        match kind {
            GotoKind::References => self.proj.refs_term = word,
            GotoKind::TypeDefinition => self.proj.goto_term = None,
            GotoKind::Definition | GotoKind::Implementation => self.proj.goto_term = word,
        }
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
            self.proj.refs_encoding = client.encoding;
            self.proj.search_seq
        } else {
            self.proj.goto_seq += 1;
            self.proj.goto_encoding = client.encoding;
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
                self.status = tr!(
                    "{count} definitions — pick one in the Search list",
                    count = targets.len()
                );
                self.proj.nav_term = self.proj.goto_term.clone();
                let title = match self.proj.nav_term.as_deref() {
                    Some(name) => tr!("Definitions of “{name}”", name = name),
                    None => tr!("Definitions").to_string(),
                };
                self.show_locations(&title, targets, self.proj.goto_encoding)
            }
            Ok(targets) if !targets.is_empty() => {
                let t = &targets[0];
                let abs = t.path.clone();
                let target_line = t.line + 1;
                // Clear the "Looking up definition…" progress; the jump itself
                // is the feedback (otherwise the status stays stuck on it).
                self.status.clear();
                self.proj.landing_next = Some((
                    abs.clone(),
                    t.line,
                    crate::app::state::LandingAt::Char(t.character, self.proj.goto_encoding),
                ));
                self.open_file(abs, Some(target_line), true)
            }
            Ok(_) => {
                self.status = tr!("No definition found").into();
                Task::none()
            }
            Err(e) => {
                self.status = tr!("Definition failed: {error}", error = e);
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
                self.status = trn!(
                    "1 reference — showing it in Search",
                    "{n} references — showing them in Search",
                    refs.len()
                );
                self.proj.nav_term = self.proj.refs_term.clone();
                let title = match self.proj.nav_term.as_deref() {
                    Some(name) => tr!("References to “{name}”", name = name),
                    None => tr!("References").to_string(),
                };
                self.show_locations(&title, refs, self.proj.refs_encoding)
            }
            Ok(_) => {
                self.status = tr!("No references").into();
                Task::none()
            }
            Err(e) => {
                self.status = tr!("References failed: {error}", error = e);
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
        enc: viewer::PositionEncoding,
    ) -> Task<Message> {
        // Painting the sidebar supersedes whatever it was waiting for — a
        // server-side text search included (see `goto_request`), and a
        // pause in typing still to end, whose search painted over the list.
        self.proj.search_seq += 1;
        self.proj.link.pending_search = None;
        self.proj.search.typed += 1;
        let seq = self.proj.search_seq;
        let mut to_read: Vec<(usize, PathBuf, usize, usize)> = Vec::new();
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
                    // The line as the file has it, where the server's
                    // character counts: the view's has its tabs spaced out.
                    .and_then(|v| v.source_line(t.line))
                    .map(|line| location_preview(line, Some(t.character), enc));
                if from_pane.is_none() {
                    to_read.push((i, t.path.clone(), t.line, t.character));
                }
                SearchHit {
                    abs: t.path,
                    rel,
                    line: t.line + 1,
                    preview: from_pane.unwrap_or_default(),
                }
            })
            .collect();
        self.proj.search.listing = Some(label.to_string());
        self.proj.search.matcher = self
            .proj
            .nav_term
            .as_deref()
            .and_then(search::PreviewMatcher::of_name);
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
        // Coloured with the lines they show now; again once the rest are
        // read (`LocationPreviews`).
        let styled = self.style_search_previews();
        if to_read.is_empty() || !self.local_project_state() {
            return styled;
        }
        let stamp = self.stamp();
        let read = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || read_location_previews(to_read, enc))
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
        );
        Task::batch([styled, read])
    }

    /// Kick off a project search from the current query and options.
    /// Search as the reader types: once typing pauses for
    /// [`LIVE_SEARCH_PAUSE`], with what the query then is. It ran only on
    /// Enter, so a query typed and left showed nothing. A query of one
    /// character, which matches most of a project, waits for Enter; a query
    /// cleared clears the results at once.
    fn search_when_typing_pauses(&mut self) -> Task<Message> {
        self.proj.search.typed += 1;
        let typed = self.proj.search.typed;
        let query = self.proj.search.query.trim();
        if query.is_empty() {
            return self.run_search();
        }
        if query.chars().count() < 2 {
            // What is listed is a longer query's: backspaced from "ab" to
            // "a", the sidebar went on showing "ab"'s matches under "a".
            self.clear_search_results();
            return Task::none();
        }
        let stamp = self.stamp();
        // Made inside the task: a timer needs the runtime, which the update
        // that asks for it is not running on.
        Task::perform(
            async { tokio::time::sleep(LIVE_SEARCH_PAUSE).await },
            move |()| {
                Message::Nav(NavMsg::SearchSettled {
                    stamp: stamp.clone(),
                    typed,
                })
            },
        )
    }

    /// Drop what the Search sidebar lists, and any search still running
    /// for it: none of it is for the query as it now stands.
    fn clear_search_results(&mut self) {
        // Every submission supersedes both search paths, including clearing
        // the query or falling back after a server request could not be sent.
        self.proj.search_seq += 1;
        self.proj.link.pending_search = None;
        self.proj.search.hits.clear();
        self.proj.search.error = None;
        self.proj.search.skipped.clear();
        self.proj.search.running = false;
        self.proj.search.ran = false;
        self.proj.search.listing = None;
        self.proj.search.matcher = None;
        self.proj.search.styled.clear();
        self.proj.search.marks.clear();
        // Nor is the count the status line still gives for them.
        if let Some(said) = self.proj.search.said.take() {
            self.settle_status(&[&said]);
        }
    }

    pub(crate) fn run_search(&mut self) -> Task<Message> {
        let Some((root, files)) = self
            .proj
            .project
            .as_ref()
            .map(|p| (p.root.clone(), p.files.clone()))
        else {
            return Task::none();
        };
        // A pause still being waited for would search again for what this
        // search is already for: Enter, or an option toggled, mid-pause ran
        // the same search twice.
        self.proj.search.typed += 1;
        self.clear_search_results();
        if self.proj.search.query.trim().is_empty() {
            return Task::none();
        }
        self.proj.search.running = true;
        self.proj.search.ran = true;
        let seq = self.proj.search_seq;
        let opts = search::SearchOptions {
            query: self.proj.search.query.trim().to_string(),
            regex: self.proj.search.regex,
            case_sensitive: self.proj.search.case_sensitive,
            whole_word: self.proj.search.whole_word,
            include: self.proj.search.include.clone(),
            exclude: self.proj.search.exclude.clone(),
        };
        self.proj.search.matcher = search::PreviewMatcher::of_search(&opts);

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
                tr!("Disconnected from the remote host — search will work again once reconnected")
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
                        error: Some(tr!("the search failed unexpectedly").into()),
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
    ) -> Task<Message> {
        self.proj.search.running = false;
        self.proj.search.error = report.error.clone();
        self.status = match &report.error {
            Some(e) => e.clone(),
            None => search_status(report.hits.len(), &report.skipped, skipped_total),
        };
        self.proj.search.said = Some(self.status.clone());
        self.proj.search.hits = report.hits;
        self.proj.search.skipped = report.skipped;
        self.style_search_previews()
    }

    /// Colour the Search sidebar's previews, each in its file's language,
    /// off this thread: a line is highlighted on its own, and two thousand
    /// of them took a noticeable while. The list is drawn plain until they
    /// arrive. A list changed meanwhile asks again, and only the colouring
    /// last asked for is taken.
    pub(crate) fn style_search_previews(&mut self) -> Task<Message> {
        self.proj.search.styled_gen += 1;
        self.proj.search.styled.clear();
        self.proj.search.marks.clear();
        let generation = self.proj.search.styled_gen;
        // Each hit with its line as an open pane has it; else, in a local
        // project, read from its file with the colours — where its marks are
        // found, as the search found them.
        let hits: Vec<PreviewToStyle> = self
            .proj
            .search
            .hits
            .iter()
            .map(|h| PreviewToStyle {
                lang: highlight::detect(&h.abs),
                preview: h.preview.clone(),
                line: self
                    .proj
                    .panes
                    .iter()
                    .flatten()
                    .find(|v| v.abs == h.abs)
                    .and_then(|v| v.source_line(h.line.saturating_sub(1)))
                    .map(str::to_string),
                abs: h.abs.clone(),
                line0: h.line.saturating_sub(1),
            })
            .collect();
        if hits.is_empty() {
            return Task::none();
        }
        let matcher = self.proj.search.matcher.clone();
        let local = self.local_project_state();
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || style_previews(hits, matcher.as_ref(), local))
                    .await
                    .unwrap_or_default()
            },
            move |(styled, marks)| {
                Message::Nav(NavMsg::PreviewsStyled {
                    stamp: stamp.clone(),
                    generation,
                    styled,
                    marks,
                })
            },
        )
    }

    pub(crate) fn refresh_finder(&mut self) {
        if self.proj.finder.is_commands() {
            self.refresh_palette();
            return;
        }
        if self.proj.finder.is_home() {
            self.refresh_suggestions();
            return;
        }
        self.proj.finder.bar_fallback = false;
        if self.proj.finder.is_bar_search() {
            self.refresh_bar();
            return;
        }
        match self.proj.finder.mode {
            FinderMode::Files => {
                if let Some(p) = &self.proj.project {
                    let files = p.files.clone();
                    let symbols = self.fresh_symbol_index();
                    let recent = self.proj.history.recent_files(RECENT_FILES);
                    self.proj.finder.refresh_files(&files, &symbols, &recent);
                }
                // No file or symbol by that name: what the command bar finds
                // for it — its commands first ("settings").
                if self.proj.finder.results.is_empty()
                    && !self.proj.finder.query.trim().is_empty()
                    && !self.proj.finder.is_line_query()
                {
                    self.proj.finder.bar_fallback = true;
                    self.refresh_bar();
                }
            }
            FinderMode::Symbols => {
                let symbols = self.fresh_symbol_index();
                self.proj.finder.refresh_symbols(&symbols);
            }
        }
    }

    pub(crate) fn on_finder_opened(&mut self, mode: FinderMode) -> Task<Message> {
        // No project, no files or symbols to find: the command bar — Open
        // Folder, Open Remote, every command — which the field over the
        // window offers to search too. Ctrl+P and the field did nothing.
        if self.proj.project.is_none() {
            return self.on_palette_opened();
        }
        self.proj.finder.open = true;
        self.proj.finder.bar = false;
        self.proj.finder.mode = mode;
        self.proj.finder.query.clear();
        self.code_focused = false; // the finder input takes focus
        self.refresh_finder();
        operation::focus(ui::finder_input_id())
    }

    pub(crate) fn on_finder_confirm(&mut self) -> Task<Message> {
        if self.proj.finder.is_commands() {
            let title = self
                .proj
                .finder
                .results
                .get(self.proj.finder.selected)
                .and_then(|i| self.palette.get(*i))
                .map(|c| c.title.clone());
            return match title {
                Some(title) => self.run_from_bar(&title),
                None => Task::none(),
            };
        }
        if self.proj.finder.is_home() {
            return match self.palette_home.get(self.proj.finder.selected).cloned() {
                Some(suggestion) => self.run_suggestion(suggestion),
                None => Task::none(),
            };
        }
        if self.proj.finder.is_bar_search() {
            let Some(&idx) = self.proj.finder.results.get(self.proj.finder.selected) else {
                return Task::none();
            };
            return match self.palette.get(idx).map(|c| c.title.clone()) {
                Some(title) => self.run_from_bar(&title),
                None => self.finder_open_index(idx - self.palette.len()),
            };
        }
        if let Some(line) = self.proj.finder.goto_line() {
            self.proj.finder.open = false;
            if let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) {
                return self.open_file(abs, Some(line), true);
            }
            return Task::none();
        }
        // `:` with no line number yet: nothing to go to (it opened the
        // first file of the project).
        if self.proj.finder.is_line_query() {
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
        self.proj.finder.bar = false;
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
                let Some(files) = self.proj.project.as_ref().map(|p| &p.files) else {
                    return Task::none();
                };
                // Past the files, the symbols the query named.
                if let Some(symbol) = idx.checked_sub(files.len()) {
                    let Some(entry) = self.proj.symbol_index.get(symbol).cloned() else {
                        return Task::none();
                    };
                    self.land_on_name(&entry.abs, entry.line, entry.name);
                    return self.open_file(entry.abs, Some(entry.line), true);
                }
                let Some(entry) = files.get(idx).cloned() else {
                    return Task::none();
                };
                self.open_file(entry.abs, None, true)
            }
            FinderMode::Symbols => {
                let Some(entry) = self.proj.symbol_index.get(idx).cloned() else {
                    return Task::none();
                };
                self.land_on_name(&entry.abs, entry.line, entry.name);
                self.open_file(entry.abs, Some(entry.line), true)
            }
        }
    }

    /// A jump to the symbol `name` on `line1` of `abs` lands on its name.
    fn land_on_name(&mut self, abs: &Path, line1: usize, name: String) {
        self.proj.landing_next = Some((
            abs.to_path_buf(),
            line1.saturating_sub(1),
            crate::app::state::LandingAt::Name(name),
        ));
    }

    /// The function/method defined exactly at `(file, line1)`, if any — recorded
    /// with a history entry so it can be re-anchored across edits.
    pub(crate) fn symbol_name_at(&self, file: &Path, line1: usize) -> Option<String> {
        self.proj
            .symbol_index_by_file
            .get(file)?
            .iter()
            .find_map(|s| (s.line == line1 && names_trail_stops(&s.kind)).then(|| s.name.clone()))
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
    /// [`clew_core::outline::entry_kind`]), `None` for every other function.
    pub fn entry_kind_of(&self, file: &Path, name: &str) -> Option<index::EntryKind> {
        self.proj.symbol_index_by_file.get(file).and_then(|syms| {
            syms.iter()
                .find_map(|s| (s.name == name).then_some(s.entry)?)
        })
    }

    /// The class an entry point belongs to for a "reached from" walk
    /// ([`projectcalls::ProjectCallGraph::paths_from_entries`]): a main,
    /// route, command or handler ranks first, a test second, and every other
    /// function is not a place execution enters. Asked of every same-named
    /// symbol in the file, as [`Self::entry_kind_of`] is (the call graph
    /// knows a function by file and name only), so the two always agree.
    pub fn entry_class_of(&self, file: &Path, name: &str) -> Option<u8> {
        if self.entry_kind_of(file, name).is_some() {
            Some(0)
        } else if self.is_test_symbol(file, name) {
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

    /// Where the text of `caller_file` is read from to find a call in it:
    /// an open pane's text, which is the file wherever it lives; else, for
    /// a LOCAL project only, its file on this machine's disk — for a remote
    /// one the path names another host, and a same-pathed local file would
    /// silently place the call in the wrong code.
    fn call_source(&self, caller_file: &Path) -> Option<CallSource> {
        if let Some(v) = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == caller_file)
        {
            return Some(CallSource::Text(v.source.clone()));
        }
        self.proj
            .project
            .as_ref()
            .filter(|_| self.local_project_state())
            .map(|p| CallSource::Disk {
                root: p.root.clone(),
                path: caller_file.to_path_buf(),
            })
    }

    /// The first 1-based line where `caller` (in `caller_file`) calls
    /// `callee`, found by re-parsing the caller's live source — here, on
    /// the update loop; a jump finds it off it ([`App::jump_to_call`]).
    #[cfg(test)]
    pub(crate) fn call_site_line(
        &self,
        caller_file: &Path,
        caller: &str,
        caller_line: usize,
        callee: &str,
    ) -> Option<usize> {
        let source = self.call_source(caller_file)?;
        call_site(caller_file, &source.read()?, caller, caller_line, callee)
    }

    /// Open the caller at its call of `callee`: the line found by parsing
    /// the caller's text off the update loop — a file of megabytes parses
    /// for a while, and this loop serves every window — or, not found, the
    /// caller's own line.
    fn jump_to_call(
        &mut self,
        caller_file: PathBuf,
        caller: String,
        caller_line: usize,
        callee: String,
    ) -> Task<Message> {
        let source = self.call_source(&caller_file);
        let stamp = self.stamp();
        Task::perform(
            async move {
                let file = caller_file.clone();
                let line = tokio::task::spawn_blocking(move || {
                    call_site(&file, &source?.read()?, &caller, caller_line, &callee)
                })
                .await
                .ok()
                .flatten();
                (caller_file, line.unwrap_or(caller_line))
            },
            move |(caller_file, line)| {
                Message::Nav(NavMsg::CallSiteFound {
                    stamp: stamp.clone(),
                    caller_file,
                    line,
                })
            },
        )
    }
}

/// The text a call is looked for in (see `App::call_source`).
enum CallSource {
    Text(Arc<String>),
    Disk { root: PathBuf, path: PathBuf },
}

impl CallSource {
    /// Its text. From disk, under the same guard as every other client-side
    /// project-source read (the symbol index, `tasks::gather_*`): the
    /// call-graph node holding the path came from a scan that can be
    /// minutes old, so the leaf may since have become a symlink pointing
    /// outside the project — whose text would be parsed as this project's
    /// code — or a FIFO. Call graph nodes are in-root by construction, so
    /// containment costs nothing.
    fn read(self) -> Option<Arc<String>> {
        match self {
            CallSource::Text(text) => Some(text),
            CallSource::Disk { root, path } => {
                clew_core::fs_scan::read_confined_capped(&root, &path, index::MAX_INDEX_FILE_BYTES)
                    .map(Arc::new)
            }
        }
    }
}

/// The first 1-based line in `source` (the text of `file`) where `caller`,
/// defined at `caller_line`, calls `callee`.
fn call_site(
    file: &Path,
    source: &str,
    caller: &str,
    caller_line: usize,
    callee: &str,
) -> Option<usize> {
    let lang = crate::highlight::detect(file)?;
    projectcalls::calls_of(source, lang)
        .into_iter()
        // The first call after the caller's definition: a same-name
        // function earlier in the file holds calls of its own.
        .filter(|cs| {
            cs.callee == callee && cs.caller.as_deref() == Some(caller) && cs.line >= caller_line
        })
        .map(|cs| cs.line)
        .min()
}

impl App {
    /// The file being read, revealed in the Files view once it comes on the
    /// sheet: its folders opened, and its row scrolled into view when it is
    /// not in it already. Only while the tree shows (the view picked, no
    /// filter): a file opened with it hidden is revealed when it shows.
    /// `None` when there is nothing to scroll.
    pub(crate) fn sync_tree_reveal(&mut self) -> Option<Task<Message>> {
        let rel = self.active_viewer()?.rel.clone();
        if self.proj.tree_revealed.as_deref() == Some(rel.as_str())
            || !self.show_left_sidebar
            || self.sidebar != SidebarTab::Files
            || !self.file_filter.trim().is_empty()
        {
            return None;
        }
        self.proj.tree_revealed = Some(rel.clone());
        for (at, _) in rel.match_indices('/') {
            self.proj.expanded.insert(rel[..at].to_string());
        }
        let tree = &self.proj.project.as_ref()?.tree;
        let row = ui::tree_row_index(tree, &self.proj.expanded, &rel)?;
        let pitch = ui::TREE_ROW_H + ui::TREE_ROW_GAP;
        let (top, bottom) = (row as f32 * pitch, (row + 1) as f32 * pitch);
        // Before it has been scrolled, it shows from its top, about as much
        // of it as the window has room for under the chrome.
        let (offset, height) = self
            .proj
            .tree_scroll
            .unwrap_or((0.0, (self.window_height - TREE_CHROME_H).max(0.0)));
        if top >= offset && bottom <= offset + height {
            return None;
        }
        Some(iced::widget::operation::scroll_to(
            ui::files_tree_id(),
            iced::widget::scrollable::AbsoluteOffset {
                x: 0.0,
                y: (top - height / 2.0).max(0.0),
            },
        ))
    }
}

impl App {
    /// The working tree's changes read anew when they may have changed, for
    /// the Files view's marks: while the tree shows (they are read for
    /// nothing else), one read at a time, another after it when something
    /// changed meanwhile. `None` when there is nothing to read.
    pub(crate) fn sync_git_status(&mut self) -> Option<Task<Message>> {
        if !self.proj.git_status_stale
            || self.proj.git_status_loading
            || !self.show_left_sidebar
            || !matches!(self.sidebar, SidebarTab::Files | SidebarTab::History)
        {
            return None;
        }
        let git = self.git_source()?;
        self.proj.git_status_stale = false;
        self.proj.git_status_loading = true;
        let stamp = self.stamp();
        Some(Task::perform(
            async move {
                git.run::<Vec<(String, char)>>(clew_protocol::GitOp::WorkingStatus)
                    .await
            },
            move |result| {
                Message::Project(ProjectMsg::GitStatusRead {
                    stamp: stamp.clone(),
                    result,
                })
            },
        ))
    }
}

impl App {
    /// What HEAD is read anew when it may have moved, for the status bar:
    /// one read at a time, another after it when it moved meanwhile.
    /// `None` when there is nothing to read.
    pub(crate) fn sync_head(&mut self) -> Option<Task<Message>> {
        if !self.proj.head_stale || self.proj.head_loading {
            return None;
        }
        let git = self.git_source()?;
        self.proj.head_stale = false;
        self.proj.head_loading = true;
        let stamp = self.stamp();
        Some(Task::perform(
            async move {
                git.run::<Option<clew_protocol::HeadName>>(clew_protocol::GitOp::Head)
                    .await
            },
            move |result| Message::Project(ProjectMsg::HeadRead { stamp, result }),
        ))
    }
}

impl App {
    /// The row the keyboard rests on in the Files view, while its tree has
    /// the keyboard: the one it was put on, while it shows; else the file
    /// being read; else the first.
    pub(crate) fn tree_cursor_rel(&self) -> Option<String> {
        if !self.tree_focused {
            return None;
        }
        let rows = ui::tree_rows(self);
        self.tree_cursor_at(&rows).map(|i| rows[i].rel.clone())
    }

    /// The tree no longer on screen while it had the keyboard — the sidebar
    /// hidden, another view picked, a filter matching nothing — has it no
    /// more, and the keys go back to the code: nothing was left holding
    /// them, and no row was drawn as where they rest.
    pub(crate) fn sync_tree_focus(&mut self) {
        if !self.tree_focused {
            return;
        }
        let shown = self.show_left_sidebar && self.sidebar == SidebarTab::Files;
        if shown && ui::tree_has_rows(self) {
            return;
        }
        self.tree_focused = false;
        if self.region == keys::Region::Tree {
            self.region = keys::Region::Code;
            self.code_focused = true;
        }
    }

    /// [`Self::tree_cursor_rel`]'s place among `rows`.
    fn tree_cursor_at(&self, rows: &[ui::TreeRow]) -> Option<usize> {
        let find = |rel: &str| rows.iter().position(|r| r.rel == rel);
        self.proj
            .tree_cursor
            .as_deref()
            .and_then(find)
            .or_else(|| self.active_viewer().and_then(|v| find(&v.rel)))
            .or((!rows.is_empty()).then_some(0))
    }

    /// How many rows of the tree show at once: what Page Up and Page Down
    /// move by.
    fn tree_page(&self) -> usize {
        let (_, height) = self
            .proj
            .tree_scroll
            .unwrap_or((0.0, (self.window_height - TREE_CHROME_H).max(0.0)));
        ((height / (ui::TREE_ROW_H + ui::TREE_ROW_GAP)) as usize)
            .saturating_sub(1)
            .max(1)
    }

    /// The row the keyboard rests on, scrolled into view.
    pub(crate) fn reveal_tree_cursor(&self) -> Task<Message> {
        let rows = ui::tree_rows(self);
        let Some(at) = self.tree_cursor_at(&rows) else {
            return Task::none();
        };
        let pitch = ui::TREE_ROW_H + ui::TREE_ROW_GAP;
        ui::reveal(
            ui::files_tree_id(),
            at as f32 * pitch,
            at as f32 * pitch + ui::TREE_ROW_H,
        )
    }

    /// A key the Files view's tree took while it has the keyboard: the
    /// keyboard walks the tree as a pointer would, a row at a time.
    pub(crate) fn on_tree_key(&mut self, key: TreeKey) -> Task<Message> {
        match key {
            TreeKey::Leave => {
                // The Esc that leaves the tree does only that (it reaches
                // the key handler too: `shell::forward_key`).
                self.escape_taken = true;
                self.code_focused = true;
                self.region = keys::Region::Code;
                return Task::none();
            }
            TreeKey::Type(c) => {
                self.file_filter.push(c);
                self.region = keys::Region::Sidebar;
                return Task::batch([
                    iced::widget::operation::focus(ui::file_filter_id()),
                    iced::widget::operation::move_cursor_to_end(ui::file_filter_id()),
                ]);
            }
            _ => {}
        }
        let rows = ui::tree_rows(self);
        let Some(at) = self.tree_cursor_at(&rows) else {
            return Task::none();
        };
        // ↑ on the first row goes back up to the filter, as ↓ came down.
        if key == TreeKey::Up && at == 0 {
            self.region = keys::Region::Sidebar;
            return Task::batch([
                iced::widget::operation::focus(ui::file_filter_id()),
                iced::widget::operation::move_cursor_to_end(ui::file_filter_id()),
            ]);
        }
        let last = rows.len() - 1;
        let row = rows[at].clone();
        let open = row.dir && self.proj.expanded.contains(&row.rel);
        let to = match key {
            TreeKey::Up => at.saturating_sub(1),
            TreeKey::Down => (at + 1).min(last),
            TreeKey::First => 0,
            TreeKey::Last => last,
            TreeKey::PageUp => at.saturating_sub(self.tree_page()),
            TreeKey::PageDown => (at + self.tree_page()).min(last),
            // A closed folder opens; an open one steps into its first row.
            TreeKey::Right if row.dir && !open => {
                self.proj.expanded.insert(row.rel.clone());
                at
            }
            TreeKey::Right
                if open
                    && rows
                        .get(at + 1)
                        .is_some_and(|next| next.rel.starts_with(&format!("{}/", row.rel))) =>
            {
                at + 1
            }
            // An open folder closes; anything else steps out to its folder.
            TreeKey::Left if open => {
                self.proj.expanded.remove(&row.rel);
                at
            }
            TreeKey::Left => match row.rel.rsplit_once('/') {
                Some((parent, _)) => rows.iter().position(|r| r.rel == parent).unwrap_or(at),
                None => at,
            },
            TreeKey::Open | TreeKey::Preview if row.dir => {
                if !self.proj.expanded.remove(&row.rel) {
                    self.proj.expanded.insert(row.rel.clone());
                }
                at
            }
            TreeKey::Open => {
                // Enter reads the file: the keyboard goes to its code.
                let task = self.update(Message::Editor(EditorMsg::OpenRel {
                    rel: row.rel.clone(),
                    line: None,
                }));
                self.region = keys::Region::Code;
                return Task::batch([task, iced::widget::operation::focus(keys::nowhere())]);
            }
            TreeKey::Preview => {
                // Space shows the file, the keyboard staying in the tree.
                let task = self.update(Message::Editor(EditorMsg::OpenRel {
                    rel: row.rel.clone(),
                    line: None,
                }));
                self.code_focused = false;
                self.proj.tree_cursor = Some(row.rel);
                return task;
            }
            _ => at,
        };
        self.proj.tree_cursor = Some(rows[to].rel.clone());
        self.reveal_tree_cursor()
    }
}

/// About how much of the window's height is not the Files view's tree: the
/// top bar, the sidebar's tabs and filter, the status bar.
const TREE_CHROME_H: f32 = 160.0;

impl App {
    /// Handle a [`NavMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_nav(&mut self, message: NavMsg) -> Task<Message> {
        match message {
            NavMsg::SearchQueryChanged(query) => {
                self.proj.search.query = query;
                self.search_when_typing_pauses()
            }
            NavMsg::SearchSettled { typed, .. } => {
                if typed == self.proj.search.typed {
                    self.run_search()
                } else {
                    Task::none()
                }
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
                self.search_when_typing_pauses()
            }
            NavMsg::SearchExcludeChanged(s) => {
                self.proj.search.exclude = s;
                self.search_when_typing_pauses()
            }
            NavMsg::SearchSubmitted => self.run_search(),
            // Back to the search, as it was asked.
            NavMsg::ListingClosed => {
                self.clear_search_results();
                if self.proj.search.query.trim().is_empty() {
                    Task::none()
                } else {
                    self.run_search()
                }
            }
            NavMsg::SearchDone { seq, result, .. } => {
                // Only the latest submission may paint the Search sidebar.
                if seq == self.proj.search_seq {
                    let skipped_total = result.skipped.len();
                    return self.apply_search_result(result, skipped_total);
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
            NavMsg::FinderPick { abs, line, name } => {
                self.proj.finder.open = false;
                if let (Some(line), Some(name)) = (line, name) {
                    self.land_on_name(&abs, line, name);
                }
                self.open_file(abs, line, true)
            }
            NavMsg::FinderConfirm => self.on_finder_confirm(),
            NavMsg::PaletteOpened => self.on_palette_opened(),
            NavMsg::PaletteRun(title) => self.run_from_bar(&title),
            NavMsg::PaletteSuggestion(suggestion) => self.run_suggestion(suggestion),
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
                    // Coloured again with the lines they now show.
                    return self.style_search_previews();
                }
                Task::none()
            }
            NavMsg::PreviewsStyled {
                generation,
                styled,
                marks,
                ..
            } => {
                let search = &mut self.proj.search;
                if generation == search.styled_gen && styled.len() == search.hits.len() {
                    search.styled = styled;
                    if marks.len() == search.hits.len() {
                        search.marks = marks;
                    }
                }
                Task::none()
            }
            NavMsg::JumpToCall {
                caller_file,
                caller,
                caller_line,
                callee,
            } => self.jump_to_call(caller_file, caller, caller_line, callee),
            NavMsg::CallSiteFound {
                caller_file, line, ..
            } => self.open_file(caller_file, Some(line), true),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    /// `call_site_line` falls back to this machine's disk when the caller is
    /// not open in a pane, and the path it reads comes from a call graph built
    /// off a scan that can be minutes old. Both refusals matter, and neither
    /// existed before: a leaf swapped for a symlink out of the project put
    /// another file's call sites under this project's name, and an over-cap
    /// file was pulled whole into the iced update loop that serves EVERY
    /// window (a FIFO there froze the entire interface).
    #[cfg(unix)]
    #[test]
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
            app.call_site_line(&plain, "caller", 1, "callee"),
            Some(2),
            "an ordinary in-project caller must still resolve"
        );
        assert_eq!(
            app.call_site_line(&link, "caller", 1, "callee"),
            None,
            "a symlink pointing out of the project was followed"
        );
        assert_eq!(
            app.call_site_line(&big, "caller", 1, "callee"),
            None,
            "a file past the index cap was read whole on the update loop"
        );

        // Two same-name callers in one file (`Item.total`, `Cart.total`):
        // each row jumps to its own function's call, by where it is defined.
        let twins = dir.join("src/cart.py");
        std::fs::write(
            &twins,
            "class Item:\n    def total(self):\n        return helper(1)\n\n\
             class Cart:\n    def total(self):\n        return helper(2)\n",
        )
        .unwrap();
        assert_eq!(app.call_site_line(&twins, "total", 2, "helper"), Some(3));
        assert_eq!(app.call_site_line(&twins, "total", 6, "helper"), Some(7));

        // A jump along the call edge finds the line off the update loop: the
        // jump itself parses nothing, and what comes back names the line —
        // the caller's own when the call is not found.
        for (caller_line, found) in [(6, 7), (1, 1)] {
            let jump = app.update(Message::Nav(NavMsg::JumpToCall {
                caller_file: twins.clone(),
                caller: "total".into(),
                caller_line,
                callee: if caller_line == 6 { "helper" } else { "absent" }.into(),
            }));
            let landed: Vec<usize> = crate::app::tests::run_task(jump)
                .into_iter()
                .filter_map(|m| match m {
                    Message::Nav(NavMsg::CallSiteFound { line, .. }) => Some(line),
                    _ => None,
                })
                .collect();
            assert_eq!(landed, [found]);
        }

        for d in [&dir, &outside_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[cfg(test)]
mod location_tests {
    #[cfg(unix)]
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
    #[cfg(unix)]
    #[test]
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
            viewer::PositionEncoding::Utf16,
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
        let stale = app.show_locations(
            "(references)",
            vec![target(&plain, 0)],
            viewer::PositionEncoding::Utf16,
        );
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
