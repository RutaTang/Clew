//! The Cmd-hover peek and the right-click context menu: dwell debouncing, the
//! local and language-server peek text, the diagnostic under the cursor, and
//! the menu that anchors the go-to / explain / debug actions.
//!
//! Its messages, [`HoverMsg`], arrive through `App::update_hover`.

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn on_hover_requested(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        // The cursor is inside the tooltip — leave it be so it can be read
        // and scrolled.
        if self.proj.hover_pinned {
            return Task::none();
        }
        // The code view reports the cursor in the scrollable's *content*
        // space (offset by the scroll, on both axes); the tooltip overlay
        // lives in window space, so remove the pane's scroll to anchor it at
        // the cursor rather than that far below / right of it.
        let (sx, sy) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or((0.0, 0.0), |v| (v.scroll_x, v.scroll_y));
        let (x, y) = (x - sx, y - sy);
        // Back on the word the open peek is of: it stays, where it is, and
        // what the pointer crossed on its way back shows nothing.
        let leave = match &self.proj.hover {
            Some(h) if self.on_peeked(pane, h, line, col) => {
                self.proj.hover_leaving = false;
                // Its very spot asked again (the view scrolled under the
                // pointer): moved to the pointer. Elsewhere on its word, it
                // stays put rather than jump about.
                if let Some(h) = &mut self.proj.hover
                    && (h.line, h.col) == (line, col)
                {
                    h.x = x;
                    h.y = y;
                }
                return Task::none();
            }
            // Off it, with nothing shown yet (the server's answer still on
            // its way): let go at once, the answer with it.
            Some(h) if !h.says_something() => {
                self.proj.hover = None;
                Task::none()
            }
            // Off it: the peek goes a moment later, as the pointer moves on —
            // not only once it rests on something else, which held a stale
            // peek over the code for as long as the pointer kept moving. The
            // moment is the pointer's to reach the peek and keep it.
            Some(_) if !self.proj.hover_leaving => {
                self.proj.hover_leaving = true;
                self.proj.hover_leave = self.proj.hover_leave.wrapping_add(1);
                let leave = self.proj.hover_leave;
                let stamp = self.stamp();
                Task::perform(
                    async move { tokio::time::sleep(HOVER_LEAVE_GRACE).await },
                    move |_| {
                        Message::Hover(HoverMsg::Left {
                            stamp: stamp.clone(),
                            leave,
                        })
                    },
                )
            }
            _ => Task::none(),
        };
        // Info shows when asked for (`peek_on_rest` off): the pointer here
        // with ⌥ (Alt) held — soon, as asked — and not otherwise; where it
        // is, kept, for ⌥ pressed now to show it at once.
        self.proj.hover_point = Some((pane, line, col, x, y));
        let delay = match (self.peek_on_rest, self.modifiers.alt()) {
            (_, true) => HOVER_KEY_DELAY,
            (true, false) => self.peek_delay(std::time::Instant::now()),
            (false, false) => {
                // Moving on stops a peek on its way for the spot left.
                self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
                return leave;
            }
        };
        // New token: start a dwell so moving across code doesn't flash
        // tooltips — it shows only if the cursor rests here for a moment.
        self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
        let hover_gen = self.proj.hover_gen;
        let stamp = self.stamp();
        let dwell = Task::perform(async move { tokio::time::sleep(delay).await }, move |_| {
            Message::Hover(HoverMsg::Dwell {
                stamp: stamp.clone(),
                hover_gen,
                pane,
                line,
                col,
                x,
                y,
            })
        });
        Task::batch([leave, dwell])
    }

    /// ⌥ (Alt) pressed or let go: pressed with the pointer resting on the
    /// code, its peek shows at once, as asked. Let go, what is shown stays —
    /// to be read, and moved into and scrolled — until the pointer leaves it.
    pub(crate) fn on_alt_changed(&mut self, held: bool) -> Task<Message> {
        if !held || self.proj.hover_pinned {
            return Task::none();
        }
        let Some((pane, line, col, x, y)) = self.proj.hover_point else {
            return Task::none();
        };
        if self
            .proj
            .hover
            .as_ref()
            .is_some_and(|h| self.on_peeked(pane, h, line, col))
        {
            return Task::none();
        }
        self.proj.hover = None;
        self.proj.hover_leaving = false;
        self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
        let hover_gen = self.proj.hover_gen;
        self.on_hover_dwell(hover_gen, pane, line, col, x, y)
    }

    /// Show Info (⌘K, or `K` in the code): the peek of the word at the
    /// caret, at once, under its line — or, open, put away.
    pub(crate) fn toggle_peek_at_caret(&mut self) -> Task<Message> {
        if self.proj.hover.is_some() {
            self.invalidate_hover();
            return Task::none();
        }
        let Some((pane, line, col, x, y)) = self.caret_point() else {
            return Task::none();
        };
        self.proj.hover_pinned = false;
        self.proj.hover_leaving = false;
        self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
        let hover_gen = self.proj.hover_gen;
        self.on_hover_dwell(hover_gen, pane, line, col, x, y)
    }

    /// The caret of the code on screen, as a point under its line in the
    /// window: its pane, line and column, and where a menu or a peek opened
    /// from the keys stands. `None` with no caret, or with the code under a
    /// page (Weak Spots, a doc page) or the large Ask window.
    fn caret_point(&self) -> Option<(usize, usize, usize, f32, f32)> {
        if crate::ui::pane_cover(self).is_some() || self.ask_view == crate::AskView::Expanded {
            return None;
        }
        let pane = self.proj.active;
        let v = self.active_viewer()?;
        let (line, col) = v.caret?;
        let panes = crate::ui::panes_rect(self)?;
        // The pane's half, where the panes are split.
        let (left, width) = if self.proj.split {
            let half = panes.width / 2.0;
            (panes.x + if pane == 1 { half } else { 0.0 }, half)
        } else {
            (panes.x, panes.width)
        };
        let size = self.font_size;
        let lh = self.line_height();
        // The code's gutter and a column of its font, as the view sets
        // them (`editor::codeview`): close enough to open by the caret, and
        // kept inside the pane.
        let gutter = (48.0 + 20.0) * size / 13.0;
        let column = size * 0.6;
        let x = left + gutter + col as f32 * column - v.scroll_x;
        let y = panes.y + (v.row_of(line) + 1) as f32 * lh - v.scroll_y;
        let x = x.clamp(left, (left + width - 1.0).max(left));
        let y = y.clamp(panes.y, (panes.y + panes.height - 1.0).max(panes.y));
        Some((pane, line, col, x, y))
    }

    /// How long the pointer rests on a word, at `now`, before its peek
    /// shows: [`HOVER_DELAY`] — long enough that a pointer crossing the code
    /// on its way somewhere opens no card over it — or, reading along from
    /// a peek showing now or a moment ago ([`HOVER_WARM`]), the shorter
    /// [`HOVER_WARM_DELAY`]. A peek showing marks the moment.
    pub(crate) fn peek_delay(&mut self, now: std::time::Instant) -> std::time::Duration {
        if self
            .proj
            .hover
            .as_ref()
            .is_some_and(HoverState::says_something)
        {
            self.proj.hover_seen = Some(now);
        }
        match self.proj.hover_seen {
            Some(seen) if now.saturating_duration_since(seen) < HOVER_WARM => HOVER_WARM_DELAY,
            _ => HOVER_DELAY,
        }
    }

    /// Whether `(line, col)` of `pane` is on what the open peek `h` is of:
    /// its word, anywhere along it — or, off a word, its very spot.
    fn on_peeked(&self, pane: usize, h: &HoverState, line: usize, col: usize) -> bool {
        if h.line != line {
            return false;
        }
        h.col == col
            || self
                .proj
                .panes
                .get(pane)
                .and_then(Option::as_ref)
                .and_then(|v| analyze::word_range_at(&v.lines, line, h.col))
                .is_some_and(|(start, end)| (start..end).contains(&col))
    }

    /// The grace after the pointer leaves the peeked word ([`HOVER_LEAVE_GRACE`])
    /// is over: the peek goes, unless the pointer is back on the word or in
    /// the peek — or this is an earlier leaving's.
    fn on_hover_left(&mut self, leave: u64) -> Task<Message> {
        if leave == self.proj.hover_leave && self.proj.hover_leaving && !self.proj.hover_pinned {
            self.proj.hover = None;
            self.proj.hover_leaving = false;
        }
        Task::none()
    }

    /// The pointer moved onto gutter line `line` of `pane` (or off the
    /// gutter): its tip, if the line has a breakpoint or a bookmark, shows
    /// once the pointer has rested there, as every tooltip does.
    pub(crate) fn on_gutter_hover(
        &mut self,
        pane: usize,
        line: Option<usize>,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        let Some(line) = line else {
            self.proj.gutter_tip = None;
            return Task::none();
        };
        // In the window, as the peek is placed: the code reports the
        // pointer in its scrolled space.
        let (sx, sy) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or((0.0, 0.0), |v| (v.scroll_x, v.scroll_y));
        self.proj.gutter_seq = self.proj.gutter_seq.wrapping_add(1);
        let generation = self.proj.gutter_seq;
        self.proj.gutter_tip = Some(crate::app::state::GutterTip {
            pane,
            line,
            x: x - sx,
            y: y - sy,
            scroll: (sx, sy),
            generation,
            shown: false,
        });
        let stamp = self.stamp();
        Task::perform(
            async { tokio::time::sleep(GUTTER_TIP_DELAY).await },
            move |_| {
                Message::Hover(HoverMsg::GutterDue {
                    stamp: stamp.clone(),
                    generation,
                })
            },
        )
    }

    pub(crate) fn on_hover_dwell(
        &mut self,
        hover_gen: u64,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        if hover_gen != self.proj.hover_gen || self.proj.hover_pinned {
            return Task::none(); // cursor moved on, or is inside the tooltip
        }
        // Back on the open peek's word since: this spot was only crossed.
        if self.proj.hover.is_some() && !self.proj.hover_leaving {
            return Task::none();
        }
        self.proj.hover_leaving = false;
        // Rested past the line's end, over nothing: no word is under the
        // pointer, and none is peeked at — the hit there is the line's end,
        // and the peek of its last word opened over the blank space beside
        // it, covering the code below. What was peeked at goes once the
        // pointer rests here, as when it leaves the code; until then it
        // stays, for the pointer crossing blank space on its way into it.
        let past_end = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .is_some_and(|v| col >= v.line_len(line));
        if past_end {
            self.proj.hover = None;
            return Task::none();
        }
        let stamp = self.stamp();
        self.proj.hover = Some(HoverState {
            line,
            col,
            x,
            y,
            text: None,
            prepared: Vec::new(),
            // The Explain one-liner is cached, so attach it synchronously;
            // any LSP text arrives later and renders below it.
            summary: self.hover_summary(pane, line, col),
            // The diagnostic under the cursor (if the symbol is
            // underlined), so the hover explains the error.
            diagnostic: self.diagnostic_at(pane, line, col),
        });
        // Debug: while paused, hovering an identifier shows its live value
        // (evaluated in the current frame, as the `"hover"` context) instead
        // of LSP info — by default only when the adapter promised that such an
        // evaluation has no side effects, otherwise only after the reader
        // opted in (see `dap::hover_eval_allowed`).
        if dap::hover_eval_allowed(self.debug.hover_safe, self.debug_hover_eval)
            && self.real_run_shown()
            && let Some(session) = self
                .debug
                .session
                .as_ref()
                .filter(|s| s.status == DebugStatus::Stopped)
            && let (Some(client), Some(frame)) =
                (session.client.clone(), self.evaluation_frame(session))
        {
            let frame_id = frame.id;
            if let Some(word) = self
                .proj
                .panes
                .get(pane)
                .and_then(Option::as_ref)
                .and_then(|v| analyze::word_at(&v.lines, line, col))
            {
                let w = word.clone();
                return Task::perform(
                    async move {
                        client
                            .evaluate(&word, frame_id, dap::EvalContext::Hover)
                            .await
                    },
                    move |res| {
                        Message::Hover(HoverMsg::Answered {
                            stamp: stamp.clone(),
                            hover_gen,
                            line,
                            col,
                            text: res
                                .ok()
                                .filter(|v| !v.is_empty())
                                .map(|v| format!("{w} = {v}")),
                            markdown: false,
                        })
                    },
                );
            }
        }
        // Local peek (tree-sitter only): the same-file symbol's doc
        // comment and/or the Rust type's structure. Instant, no LSP
        // round-trip, and works with no server configured at all. Without a
        // server it is the answer — which is why the structure index has to
        // track disk (see `request_structure_build`) rather than being built
        // once per project open: a name only the stale index still knew — a
        // type deleted during the session — answered with its old relations.
        let local = self.local_peek(pane, line, col);
        let ready = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| v.lsp_lang())
            .is_some_and(|lang| matches!(self.proj.link.lsp.get(lang), Some(LspSlot::Ready(_))));
        // The peek is Markdown — a Python docstring's sections, a Rust doc
        // comment's code — and set as the server's answer is: shown as plain
        // text, its `**Args**` and backticks were read as written.
        let mut highlight = Task::none();
        if let Some(text) = local {
            let (prepared, task) = self.prepare_segments(&text);
            if let Some(h) = &mut self.proj.hover {
                h.text = None;
                h.prepared = prepared;
            }
            if !ready {
                return task;
            }
            highlight = task;
        }
        // With a server, it is asked all the same, and its answer replaces
        // the peek: the peek knows the token's name, the server which symbol
        // it is — a call of another type's `len` showed this file's `len`.
        // The project's structure of the type stays under its answer.
        let structure = self.local_structure(pane, line, col);
        // Pull the request context before mutating self further.
        let Some((lang, path, source_line)) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| {
                v.lsp_lang().map(|l| {
                    (
                        l,
                        v.lsp_path(),
                        v.source_line(line).unwrap_or("").to_string(),
                    )
                })
            })
        else {
            return Task::none();
        };
        let client = match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => return highlight,
        };
        // The display column, in the server's own position encoding.
        let character = viewer::Col(col).to_offset(&source_line, client.encoding);
        let asked = Task::perform(
            async move { client.hover(&path, line, character).await },
            move |result| {
                Message::Hover(HoverMsg::Answered {
                    stamp: stamp.clone(),
                    hover_gen,
                    line,
                    col,
                    text: result.ok().flatten().map(|text| match &structure {
                        Some(structure) => format!("{text}\n\n```text\n{structure}\n```"),
                        None => text,
                    }),
                    markdown: true,
                })
            },
        );
        Task::batch([highlight, asked])
    }

    pub(crate) fn on_hover_cleared(&mut self) -> Task<Message> {
        // Cursor left the code area — drop the peek and cancel any pending
        // dwell (a stale HoverMsg::Dwell will see the bumped gen and no-op).
        // But not if it's inside the tooltip (which overlaps the code).
        self.proj.hover_point = None;
        if !self.proj.hover_pinned {
            self.proj.hover = None;
            self.proj.hover_leaving = false;
            self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
        }
        Task::none()
    }

    /// A gutter's tip put away: its pane's file replaced, or the pane gone
    /// (`None`: any pane's). Only the gutter itself put it away, as the
    /// pointer left it, and a pane gone has no gutter to.
    pub(crate) fn forget_gutter_tip(&mut self, pane: Option<usize>) {
        if self
            .proj
            .gutter_tip
            .as_ref()
            .is_some_and(|tip| pane.is_none_or(|p| p == tip.pane))
        {
            self.proj.gutter_tip = None;
        }
    }

    /// The document under the cursor changed, so every hover in flight is
    /// about a file that is no longer there. Drops the peek and bumps the
    /// generation so a late `HoverMsg::Answered` is recognized as stale.
    ///
    /// Unlike [`Self::on_hover_cleared`] this ignores `hover_pinned` and
    /// clears it: the tooltip widget only exists while `hover` is `Some`, so
    /// a pin left set could never be released by the mouse leaving it, and
    /// would suppress every later hover.
    pub(crate) fn invalidate_hover(&mut self) {
        self.proj.hover = None;
        self.proj.hover_pinned = false;
        self.proj.hover_leaving = false;
        self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
    }

    /// Hover peek assembled locally, no LSP: the same-file symbol's doc comment
    /// plus, for Rust, the project-wide structure of the type or trait under the
    /// cursor ("impl …" / "Implementors …"). `None` when neither applies, so the
    /// caller falls through to the language server.
    pub(crate) fn local_peek(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.proj.panes.get(pane)?.as_ref()?;
        let word = analyze::word_at(&v.lines, line, col)?;
        let mut parts: Vec<String> = Vec::new();
        // The author's doc comment, if `word` names a symbol defined here.
        if let Some(sym_line) = v.symbols.iter().find(|s| s.name == word).map(|s| s.line)
            && let Some(doc) = v.docs.get(&sym_line)
        {
            parts.push(doc.clone());
        }
        // The type's relations as they are, not as Markdown: set as the
        // server's answer sets them.
        parts.extend(
            self.local_structure(pane, line, col)
                .map(|structure| format!("```text\n{structure}\n```")),
        );
        // A table or a column of the project's schema.
        parts.extend(
            self.schema_hit(pane, line, col)
                .and_then(|hit| self.schema_peek(hit)),
        );
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The Rust type's or trait's relations under `(line, col)`, resolved
    /// project-wide ("impl …" / "Implementors …").
    fn local_structure(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.proj.panes.get(pane)?.as_ref()?;
        if v.lang_key != Some("rust") {
            return None;
        }
        let word = analyze::word_at(&v.lines, line, col)?;
        self.proj.structure.summary_line(&word)
    }

    /// The cached one-line Explain summary for the identifier under `(line, col)`,
    /// if it names an explained function/method. Prefers a definition in the same
    /// file, then a unique match anywhere in the project (so hovering a call to a
    /// function defined elsewhere still shows what it does). `None` when the name
    /// is unknown, ambiguous, or its summary is an error placeholder.
    /// The cached explanation to show on hover: the full summary (the tooltip
    /// wraps and scrolls, so no first-sentence truncation here).
    pub(crate) fn hover_summary(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.proj.panes.get(pane)?.as_ref()?;
        let usable = |c: &explain::Cached| {
            (!explain::is_error_summary(&c.summary))
                .then(|| crate::app::explain::shown_summary(c).into_owned())
        };
        if let Some(word) = analyze::word_at(&v.lines, line, col) {
            // Same-file definition wins (unambiguous).
            // Same-file definition wins; a word can't say which same-name
            // overload it means, so take the first (ordinal 0).
            if let Some(c) = self.proj.explain.cache.get(&explain::Node::Function {
                file: v.abs.clone(),
                name: word.clone(),
                ordinal: 0,
            }) {
                return usable(c);
            }
            // Otherwise, only if exactly one explained function has this name.
            let mut hit: Option<&explain::Cached> = None;
            let mut ambiguous = false;
            for (node, c) in &self.proj.explain.cache {
                if let explain::Node::Function { name, .. } = node
                    && name == &word
                {
                    if hit.is_some() {
                        ambiguous = true;
                        break;
                    }
                    hit = Some(c);
                }
            }
            if !ambiguous && let Some(c) = hit {
                return usable(c);
            }
            // A project term — a type, module or acronym the project defines
            // in its own words (the glossary) — reads as itself wherever it
            // is met. One defined in this very file is left to the local
            // peek, which shows its whole doc comment. A module is met where
            // a line imports it: elsewhere a word like `config` is far more
            // often a local of that name than the module.
            if let Some(term) = self.glossary().lookup(&word)
                && term.rel != v.rel
                && (term.kind != crate::app::glossary::TermKind::Module
                    || crate::app::glossary::imports_on(v.source_line(line).unwrap_or("")))
            {
                return Some(term.peek_line());
            }
        }
        // Anywhere on a function's signature line reads as hovering that
        // function — this replaces the old end-of-line inline summary chip.
        let sig = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .find(|s| s.line == line + 1)?;
        let c = self.proj.explain.cache.get(&explain::Node::Function {
            file: v.abs.clone(),
            name: sig.name.clone(),
            ordinal: outline::fn_ordinal(&v.symbols, sig),
        })?;
        usable(c)
    }

    /// The LSP diagnostic covering (`line`, `col`) in `pane`, as a labelled
    /// message ("Error: …" / "Warning: …"), so hovering a red-underlined symbol
    /// says what is wrong. Prefers the most severe diagnostic at that spot. Uses
    /// the same char→display-column mapping as the underline rendering.
    pub(crate) fn diagnostic_at(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.proj.panes.get(pane)?.as_ref()?;
        let lang = v.lsp_lang()?;
        // The language's server and the one run beside it alike.
        self.file_diagnostics(lang, &v.lsp_path())?
            .into_iter()
            .filter(|(d, _)| d.line == line)
            .filter(|(d, encoding)| {
                let (c0, c1) = v.lsp_span(d.line, d.char_start, d.char_end, *encoding);
                (c0..c1).contains(&viewer::Col(col))
            })
            // Severity 1 is error (most severe) → lowest number sorts first.
            .min_by_key(|(d, _)| d.severity)
            .map(|(d, _)| {
                // The server's message as it sent it, under our label, and
                // who said it under what code (`rustc(E0308)`): what to look
                // it up by.
                let message = match d.origin() {
                    Some(origin) => format!("{} {origin}", d.message.trim()),
                    None => d.message.trim().to_string(),
                };
                let message = message.as_str();
                match d.severity {
                    1 => tr!("Error: {message}", message = message),
                    2 => tr!("Warning: {message}", message = message),
                    3 => tr!("Info: {message}", message = message),
                    _ => tr!("Hint: {message}", message = message),
                }
            })
    }

    /// A line's ▶ was clicked: the code menu opens there in its short form
    /// — running and debugging what starts on the line, and the breakpoint
    /// the ▶ stands in for.
    pub(crate) fn on_run_mark_clicked(
        &mut self,
        pane: usize,
        line: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        if pane == 0 || self.proj.split {
            self.proj.active = pane;
        }
        let (sx, sy) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or((0.0, 0.0), |v| (v.scroll_x, v.scroll_y));
        let main = self.main_at(pane, line);
        let test = self.test_at(pane, line);
        if main.is_none() && test.is_none() {
            return Task::none();
        }
        // Lean has no debugger.
        let debug = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .is_none_or(|v| v.lang_key != Some("lean"));
        self.proj.tree_menu = None;
        self.proj.gutter_tip = None;
        self.proj.context_menu = Some(ContextMenu {
            pane,
            line: line.saturating_sub(1),
            col: 0,
            x: x - sx,
            y: y - sy,
            insight: None,
            served: self.served(pane),
            by_pointer: true,
            line_top: None,
            test,
            gutter: Some(crate::GutterRun { main, debug }),
        });
        Task::none()
    }

    pub(crate) fn on_context_menu_opened(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        if pane == 0 || self.proj.split {
            self.proj.active = pane;
        }
        // Content space → window space (see HoverMsg::Requested): drop the
        // pane's scroll on both axes so the menu opens at the click.
        let (sx, sy) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or((0.0, 0.0), |v| (v.scroll_x, v.scroll_y));
        let (x, y) = (x - sx, y - sy);
        let insight = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| v.lang_key)
            .and_then(InsightKind::for_lang);
        let served = self.served(pane);
        // One menu at a time: the tree's would come back when this closes.
        self.proj.tree_menu = None;
        self.proj.context_menu = Some(ContextMenu {
            pane,
            line,
            col,
            x,
            y,
            insight,
            served,
            by_pointer: true,
            line_top: None,
            test: self.test_at(pane, line + 1),
            gutter: None,
        });
        // The menu acts where it was opened: the caret goes there, as a
        // click puts it — the code said nothing of what it would act on. A
        // selection clicked inside stays, for what acts on it (Add to Ask).
        if let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) {
            let at = (line, col);
            let inside = v.selection.is_some_and(|(a, b)| {
                let (from, to) = if a <= b { (a, b) } else { (b, a) };
                from != to && from <= at && at <= to
            });
            if !inside {
                v.selection = Some((at, at));
            }
            v.caret = Some(at);
            self.reader_moved_caret(pane);
            return self.sync_reading_context();
        }
        Task::none()
    }

    /// ⇧F10 or the Menu key in the code: its menu at the caret, as a
    /// right-click there opens it — the menu was the pointer's alone.
    pub(crate) fn open_context_menu_at_caret(&mut self) -> Task<Message> {
        // Not for code that is not on screen: a page over it (Weak Spots,
        // the statistics, a doc page), or the large Ask window. The menu
        // opened over the page, for a line no one could see.
        let Some((pane, line, col, x, y)) = self.caret_point() else {
            return Task::none();
        };
        let Some(panes) = crate::ui::panes_rect(self) else {
            return Task::none();
        };
        let lh = self.line_height();
        let Some((sx, sy)) = self.active_viewer().map(|v| (v.scroll_x, v.scroll_y)) else {
            return Task::none();
        };
        // In the content's space, as a click's point comes.
        let opened = self.on_context_menu_opened(pane, line, col, x + sx, y + sy);
        // Where its line starts: with no room below it, the menu opens
        // above the line, not over it.
        if let Some(menu) = self.proj.context_menu.as_mut() {
            menu.line_top = Some((y - lh).max(panes.y));
        }
        // Opened from the keys, on its first row: Enter runs it. With
        // nothing chosen, Enter did nothing until ↓ was pressed.
        let ring = self.on_tab(true, false);
        Task::batch([opened, ring])
    }

    /// Whether the language server of what `pane` shows is ready.
    pub(crate) fn served(&self, pane: usize) -> bool {
        self.proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .and_then(|v| v.lsp_lang())
            .is_some_and(|lang| matches!(self.proj.link.lsp.get(lang), Some(LspSlot::Ready(_))))
    }
}

impl App {
    /// Handle a [`HoverMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_hover(&mut self, message: HoverMsg) -> Task<Message> {
        match message {
            HoverMsg::RunMarkClicked { pane, line, x, y } => {
                self.on_run_mark_clicked(pane, line, x, y)
            }
            HoverMsg::ContextMenuOpened {
                pane,
                line,
                col,
                x,
                y,
            } => self.on_context_menu_opened(pane, line, col, x, y),
            HoverMsg::ContextMenuClosed => {
                self.proj.context_menu = None;
                Task::none()
            }
            HoverMsg::ContextGoto(kind) => {
                let Some(menu) = self.proj.context_menu.take() else {
                    return Task::none();
                };
                self.goto_request(menu.pane, menu.line, menu.col, kind)
            }
            HoverMsg::Insight => self.on_insight_from_menu(None),
            HoverMsg::InsightOf(kind) => self.on_insight_from_menu(Some(kind)),
            HoverMsg::InsightAtCursor => self.insight_at_cursor(),
            HoverMsg::InsightAnswered { token, result, .. } => {
                self.on_insight_answered(token, result)
            }
            HoverMsg::InsightClose => {
                self.proj.insight = None;
                Task::none()
            }
            HoverMsg::InsightOpenDefinition => self.on_insight_open_definition(),
            HoverMsg::Requested {
                pane,
                line,
                col,
                x,
                y,
            } => self.on_hover_requested(pane, line, col, x, y),
            HoverMsg::Dwell {
                hover_gen,
                pane,
                line,
                col,
                x,
                y,
                ..
            } => self.on_hover_dwell(hover_gen, pane, line, col, x, y),
            HoverMsg::Answered {
                hover_gen,
                line,
                col,
                text,
                markdown,
                ..
            } => {
                // The peek this text was fetched for must still be the open
                // one; line/col stays as a cheap consistency check.
                let open = hover_gen == self.proj.hover_gen
                    && self
                        .proj
                        .hover
                        .as_ref()
                        .is_some_and(|h| h.line == line && h.col == col);
                // No answer leaves the local peek shown, if there is one.
                let Some(text) = text.filter(|_| open) else {
                    return Task::none();
                };
                // rust-analyzer leaves an intra-doc link it could not resolve
                // as written, `[Subscriber]`: shown as rustdoc shows it.
                let rust = self.active_viewer().and_then(|v| v.lang_key) == Some("rust");
                let text = match markdown && rust {
                    true => clew_core::docs::rustdoc_links(&text),
                    false => text,
                };
                let (prepared, task) = match markdown {
                    true => self.prepare_segments(&text),
                    false => (Vec::new(), Task::none()),
                };
                if let Some(h) = &mut self.proj.hover {
                    h.text = (!markdown).then_some(text);
                    h.prepared = prepared;
                }
                task
            }
            HoverMsg::Cleared => self.on_hover_cleared(),
            HoverMsg::Left { leave, .. } => self.on_hover_left(leave),
            HoverMsg::Gutter { pane, line, x, y } => self.on_gutter_hover(pane, line, x, y),
            HoverMsg::GutterDue { generation, .. } => {
                if let Some(tip) = self
                    .proj
                    .gutter_tip
                    .as_mut()
                    .filter(|t| t.generation == generation)
                {
                    tip.shown = true;
                }
                Task::none()
            }
            HoverMsg::Pin(inside) => {
                self.proj.hover_pinned = inside;
                if !inside {
                    // Left the tooltip: dismiss it and cancel any pending dwell.
                    self.proj.hover = None;
                    self.proj.hover_leaving = false;
                    self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
                }
                Task::none()
            }
        }
    }
}

/// How long the pointer rests on a word with ⌥ (Alt) held before its peek
/// shows: asked for, at once — but for a pointer sweeping across words, one
/// a word.
pub(crate) const HOVER_KEY_DELAY: std::time::Duration = std::time::Duration::from_millis(80);

/// How long the pointer rests on a word before its peek shows: as long as
/// a tooltip's ([`crate::ui::tip::DELAY`]). At 300 ms a card the size of a
/// crate's docs opened over the code wherever the pointer paused on its way.
pub(crate) const HOVER_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// How long it rests, reading along from a peek, before the next shows:
/// shorter, not at once — a code peek is a card, not a word, and one at
/// every word crossed would flash over the code.
pub(crate) const HOVER_WARM_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// How long after a peek was showing the next one waits only
/// [`HOVER_WARM_DELAY`].
pub(crate) const HOVER_WARM: std::time::Duration = std::time::Duration::from_millis(1000);

/// How long the open peek stays once the pointer leaves its word: time to
/// move into it (and keep it, to read and scroll), short enough that it does
/// not hang over the code the pointer moved on to.
pub(crate) const HOVER_LEAVE_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long the pointer rests on a gutter line before its tip shows: as
/// long as any tooltip's.
pub(crate) const GUTTER_TIP_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
