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
        // Same token already shown: just reposition.
        if let Some(h) = &mut self.proj.hover
            && h.line == line
            && h.col == col
        {
            h.x = x;
            h.y = y;
            return Task::none();
        }
        // New token: start a dwell so moving across code doesn't flash
        // tooltips — it shows only if the cursor rests here for a moment.
        // The current peek stays visible until the new one is ready, so the
        // cursor can travel down into it without it vanishing first.
        self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
        let hover_gen = self.proj.hover_gen;
        let stamp = self.stamp();
        Task::perform(
            async move { tokio::time::sleep(std::time::Duration::from_millis(300)).await },
            move |_| {
                Message::Hover(HoverMsg::Dwell {
                    stamp: stamp.clone(),
                    hover_gen,
                    pane,
                    line,
                    col,
                    x,
                    y,
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
        let stamp = self.stamp();
        self.proj.hover = Some(HoverState {
            line,
            col,
            x,
            y,
            text: None,
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
            && let Some(session) = self
                .debug
                .session
                .as_ref()
                .filter(|s| s.status == DebugStatus::Stopped)
            && let (Some(client), Some(frame)) = (session.client.clone(), session.frames.first())
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
                        })
                    },
                );
            }
        }
        // Local peek (tree-sitter only): the same-file symbol's doc
        // comment and/or the Rust type's structure. Instant, no LSP
        // round-trip, and works with no server configured at all.
        // It also SUPPRESSES the language server for this token, which is why
        // the structure index has to track disk (see `request_structure_build`)
        // rather than being built once per project open: a name only the stale
        // index still knew — a type deleted during the session — answered here
        // with its old relations instead of falling through to rust-analyzer,
        // which would have reported the identifier as unresolved.
        if let Some(text) = self.local_peek(pane, line, col) {
            if let Some(h) = &mut self.proj.hover {
                h.text = Some(text);
            }
            return Task::none();
        }
        // Pull the request context before mutating self further.
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
            _ => return Task::none(),
        };
        // The display column, in the server's own position encoding.
        let character = viewer::Col(col).to_offset(&source_line, client.encoding);
        Task::perform(
            async move { client.hover(&path, line, character).await },
            move |result| {
                Message::Hover(HoverMsg::Answered {
                    stamp: stamp.clone(),
                    hover_gen,
                    line,
                    col,
                    text: result.ok().flatten(),
                })
            },
        )
    }

    pub(crate) fn on_hover_cleared(&mut self) -> Task<Message> {
        // Cursor left the code area — drop the peek and cancel any pending
        // dwell (a stale HoverMsg::Dwell will see the bumped gen and no-op).
        // But not if it's inside the tooltip (which overlaps the code).
        if !self.proj.hover_pinned {
            self.proj.hover = None;
            self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
        }
        Task::none()
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
        // Rust type/trait relations, resolved project-wide.
        if v.lang_key == Some("rust")
            && let Some(summary) = self.proj.structure.summary_line(&word)
        {
            parts.push(summary);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
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
            // peek, which shows its whole doc comment.
            if let Some(term) = self.glossary().lookup(&word)
                && term.rel != v.rel
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
        let lang = v.lang_key?;
        let LspSlot::Ready(client) = self.proj.link.lsp.get(lang)? else {
            return None;
        };
        let Some(Ok(snapshot)) = self.proj.link.lsp_snapshots.get(lang) else {
            return None;
        };
        snapshot
            .diagnostics(&v.abs)
            .iter()
            .filter(|d| d.line == line)
            .filter(|d| {
                let (c0, c1) = v.lsp_span(d.line, d.char_start, d.char_end, client.encoding);
                (c0..c1).contains(&viewer::Col(col))
            })
            // Severity 1 is error (most severe) → lowest number sorts first.
            .min_by_key(|d| d.severity)
            .map(|d| {
                let label = match d.severity {
                    1 => "Error",
                    2 => "Warning",
                    3 => "Info",
                    _ => "Hint",
                };
                format!("{label}: {}", d.message.trim())
            })
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
        self.proj.context_menu = Some(ContextMenu {
            pane,
            line,
            col,
            x,
            y,
        });
        Task::none()
    }
}

impl App {
    /// Handle a [`HoverMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_hover(&mut self, message: HoverMsg) -> Task<Message> {
        match message {
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
                ..
            } => {
                // The peek this text was fetched for must still be the open
                // one; line/col stays as a cheap consistency check.
                if hover_gen == self.proj.hover_gen
                    && let Some(h) = &mut self.proj.hover
                    && h.line == line
                    && h.col == col
                {
                    h.text = text;
                }
                Task::none()
            }
            HoverMsg::Cleared => self.on_hover_cleared(),
            HoverMsg::Pin(inside) => {
                self.proj.hover_pinned = inside;
                if !inside {
                    // Left the tooltip: dismiss it and cancel any pending dwell.
                    self.proj.hover = None;
                    self.proj.hover_gen = self.proj.hover_gen.wrapping_add(1);
                }
                Task::none()
            }
        }
    }
}
