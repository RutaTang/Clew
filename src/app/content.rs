//! Reading context and generated content: cursor targeting, explanations, prepared segments, and the overview/walkthrough/embed/ask input gatherers.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// The explanation target for the active pane's caret: the innermost
    /// function/method it sits in, or the file itself when it's between
    /// functions. Drives the always-on explanation panel.
    pub(crate) fn cursor_target(&self) -> Option<explain::Node> {
        let v = self.active_viewer()?;
        let line1 = v.caret.map(|(l, _)| l + 1)?;
        let name = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .filter(|s| s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| s.end_line.saturating_sub(s.line))
            .map(|s| s.name.clone());
        Some(match name {
            Some(name) => explain::Node::Function {
                file: v.abs.clone(),
                name,
            },
            None => explain::Node::File(v.abs.clone()),
        })
    }

    /// Context-aware starter questions for the Ask panel, most specific first:
    /// about any pinned selection, the symbol/file under the cursor, then the
    /// codebase. Static templates — instant and free.
    pub fn suggested_questions(&self) -> Vec<String> {
        let mut qs: Vec<String> = Vec::new();
        if !self.ask_pins.is_empty() {
            qs.push("Explain the attached code.".into());
            qs.push("Why is the attached code written this way?".into());
        }
        match self.cursor_target() {
            Some(explain::Node::Function { name, .. }) => {
                qs.push(format!("What calls `{name}`?"));
                qs.push(format!("What are the edge cases in `{name}`?"));
                qs.push(format!("How does `{name}` handle errors?"));
            }
            Some(explain::Node::File(p)) => {
                let f = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("this file");
                qs.push(format!("What is the role of `{f}`?"));
                qs.push(format!("What are the key types in `{f}`?"));
            }
            _ => {}
        }
        qs.push("What is the entry point of this codebase?".into());
        qs.push("How does data flow through the app?".into());
        qs.truncate(4);
        qs
    }

    /// Point the explanation panel at the function/file under the caret. No-op if
    /// it already shows that target (so moving within one function is free).
    /// `extra` is the caller's own task (e.g. a scroll), run alongside.
    pub(crate) fn follow_caret(&mut self, extra: Task<Message>) -> Task<Message> {
        let Some(target) = self.cursor_target() else {
            return extra;
        };
        if self.explain.view.as_ref() == Some(&target) {
            return extra;
        }
        Task::batch([extra, self.show_explanation(target)])
    }

    /// Show the pre-built explanation for the innermost function/method whose
    /// span contains `line1` (1-based) in `file`. Used by the Outline Cmd+click
    /// and the code context menu. Everything is explained at project startup, so
    /// this is a pure show — no on-demand generation.
    pub(crate) fn explain_symbol_at(&mut self, file: PathBuf, line1: usize) -> Task<Message> {
        self.show_right_panel = true; // explicit action → reveal the panel
        let name = self
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
                    .map(|s| s.name.clone())
            });
        match name {
            Some(name) => {
                let node = explain::Node::Function { file, name };
                // Reveal the panel now (a cached summary, or the placeholder),
                // then generate the block walkthrough. Without the second step a
                // menu / Cmd+click "Explain" just parked the panel on "Not
                // explained yet" and looked dead; ExplainBlocks shows a cached
                // walkthrough if present, else streams a fresh one.
                let show = self.show_explanation(node.clone());
                Task::batch([show, Task::done(Message::ExplainBlocks(node))])
            }
            None => {
                self.status = "No function here to explain".into();
                Task::none()
            }
        }
    }

    /// Open the explanation overlay for `node`, showing its summary.
    pub(crate) fn show_explanation(&mut self, node: explain::Node) -> Task<Message> {
        let summary = self
            .explain
            .cache
            .get(&node)
            .map(|c| c.summary.clone())
            .unwrap_or_else(|| "Not explained yet — press Explain in the toolbar.".to_string());
        self.present(node, &summary, false)
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
        self.explain.prepared = prepared;
        self.explain.view = Some(node);
        self.explain.showing_detail = detail;
        // The call-flow strip needs the project call graph; build it lazily while
        // the reader is actually looking at a function in the context panel.
        let build = if self.show_right_panel
            && matches!(self.explain.view, Some(explain::Node::Function { .. }))
        {
            self.ensure_call_graph()
        } else {
            Task::none()
        };
        Task::batch([task, build])
    }

    /// Follow the reading cursor: keep the context panel showing the function
    /// (or, between functions, the file) the caret is in. A cheap no-op when the
    /// panel is closed or the enclosing symbol hasn't changed, so it is safe to
    /// call on every caret move. Never opens the panel on its own — that stays a
    /// deliberate act (toggle, or Cmd+click to explain).
    pub(crate) fn sync_reading_context(&mut self) -> Task<Message> {
        if !self.show_right_panel || self.split {
            return Task::none();
        }
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let abs = v.abs.clone();
        let Some((line0, _)) = v.caret else {
            return Task::none();
        };
        let line1 = line0 + 1;
        // Innermost function/method whose span contains the caret; else the file.
        let target = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .filter(|s| s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| s.end_line.saturating_sub(s.line))
            .map(|s| explain::Node::Function {
                file: abs.clone(),
                name: s.name.clone(),
            })
            .unwrap_or(explain::Node::File(abs));
        if self.explain.view.as_ref() == Some(&target) {
            return Task::none();
        }
        let show = self.show_explanation(target);
        Task::batch([show, self.outline_scroll_task()])
    }

    /// Scroll the outline so the caret's current symbol is in view (approximate —
    /// row heights are estimated — which is enough to bring it on screen). A no-op
    /// unless the caret is inside a function shown in the outline.
    pub(crate) fn outline_scroll_task(&self) -> Task<Message> {
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let name = match &self.explain.view {
            Some(explain::Node::Function { file, name }) if *file == v.abs => name.clone(),
            _ => return Task::none(),
        };
        let mut y = 0.0f32;
        let mut found = false;
        for s in &v.symbols {
            if matches!(s.kind.as_str(), "function" | "method") && s.name == name {
                found = true;
                break;
            }
            // Mirror ui::outline_content's row layout: a label line, plus a summary
            // line when inline summaries are on and this symbol has a real one.
            let mut h = 27.0;
            let has_summary = self.show_inline_summaries
                && matches!(s.kind.as_str(), "function" | "method")
                && self
                    .explain
                    .cache
                    .get(&explain::Node::Function {
                        file: v.abs.clone(),
                        name: s.name.clone(),
                    })
                    .is_some_and(|c| !explain::is_error_summary(&c.summary));
            if has_summary {
                h += 14.0;
            }
            y += h;
        }
        if !found {
            return Task::none();
        }
        let y = (y - 48.0).max(0.0); // keep a little context above the symbol
        operation::scroll_to(ui::outline_scroll_id(), AbsoluteOffset { x: 0.0, y })
    }

    /// Segment `content` (LLM markdown) for display: parse markdown, key the
    /// math/mermaid, load cached SVGs, and return a background task to render the
    /// rest. Shared by the explanation panel and the architecture overview.
    pub(crate) fn prepare_segments(&mut self, content: &str) -> (Vec<PreparedSeg>, Task<Message>) {
        let segments = richmd::segment(content);
        let root = self.project.as_ref().map(|p| p.root.clone());

        // Pull cached SVGs into memory; collect what still needs rendering.
        let mut missing: Vec<richmd::Renderable> = Vec::new();
        for r in richmd::renderables(&segments) {
            if self.explain.svgs.contains_key(&r.key) {
                continue;
            }
            let cached = root.as_ref().and_then(|rt| richmd::load_raw(rt, r.key));
            if let Some(raw) = cached {
                self.insert_svg(r.key, richmd::prepare_svg(&raw, r.kind == "math"));
            } else {
                missing.push(r);
            }
        }

        // Prepare segments for display (parse markdown once).
        let prepared = segments
            .into_iter()
            .map(|s| match s {
                richmd::Segment::Markdown(md) => {
                    PreparedSeg::Markdown(iced::widget::markdown::parse(&md).collect())
                }
                richmd::Segment::DisplayMath(tex) => {
                    PreparedSeg::DisplayMath(richmd::math_key(&tex, true))
                }
                richmd::Segment::Mermaid(src) => {
                    PreparedSeg::Mermaid(richmd::mermaid_key(&src), src)
                }
                richmd::Segment::InlineLine(parts) => PreparedSeg::InlineLine(
                    parts
                        .into_iter()
                        .map(|p| match p {
                            richmd::Inline::Text(t) => PreparedInline::Text(t),
                            richmd::Inline::Math(tex) => {
                                PreparedInline::Math(richmd::math_key(&tex, false))
                            }
                        })
                        .collect(),
                ),
            })
            .collect();

        // Render any missing diagrams/equations in the background.
        let task = match root {
            Some(root) if !missing.is_empty() => {
                self.explain.svg_gen += 1;
                let generation = self.explain.svg_gen;
                self.status = "Rendering math & diagrams…".into();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || generate_svgs(missing, root))
                            .await
                            .unwrap_or_default()
                    },
                    move |map| Message::SvgsGenerated { generation, map },
                )
            }
            _ => Task::none(),
        };
        (prepared, task)
    }

    /// Insert a prepared SVG into the session cache, building its iced handle.
    pub(crate) fn insert_svg(&mut self, key: u64, prepared: richmd::PreparedSvg) {
        self.explain.svgs.insert(
            key,
            ExplainSvg {
                handle: iced::widget::svg::Handle::from_memory(prepared.svg.into_bytes()),
                width: prepared.width,
                height: prepared.height,
            },
        );
    }

    /// Assemble the overview prompt inputs from clew's existing artifacts:
    /// folder/file summaries (the explanation cache), entry points and key types
    /// (the symbol index), and a computed module-dependency diagram (imports).
    pub(crate) fn gather_overview_inputs(&self) -> overview::Inputs {
        let root = self
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();

        // Structure: folders then files, each with its summary (rel paths so the
        // model can link them).
        let mut folders: Vec<(String, String)> = Vec::new();
        let mut files: Vec<(String, String)> = Vec::new();
        for (node, cached) in &self.explain.cache {
            match node {
                explain::Node::Folder(p) => folders.push((self.rel_of(p), cached.summary.clone())),
                explain::Node::File(p) => files.push((self.rel_of(p), cached.summary.clone())),
                explain::Node::Function { .. } => {}
            }
        }
        folders.sort();
        files.sort();
        let mut structure = String::new();
        for (rel, sum) in &folders {
            structure.push_str(&format!("📁 {rel} — {sum}\n"));
        }
        if !folders.is_empty() {
            structure.push('\n');
        }
        for (rel, sum) in &files {
            structure.push_str(&format!("{rel} — {sum}\n"));
        }

        // Entry points: functions named `main`.
        let mut entry_points: Vec<String> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .filter(|s| s.kind == "function" && s.name == "main")
            .map(|s| format!("`fn main` in {}", s.rel))
            .collect();
        entry_points.sort();
        entry_points.dedup();

        // Key types: struct/enum/class/trait symbols (capped, deterministic).
        let mut all_types: Vec<&SymbolEntry> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .filter(|s| {
                matches!(
                    s.kind.as_str(),
                    "struct" | "enum" | "class" | "trait" | "interface"
                )
            })
            .collect();
        all_types.sort_by(|a, b| a.name.cmp(&b.name).then(a.rel.cmp(&b.rel)));
        let mut seen = HashSet::new();
        let key_types: Vec<String> = all_types
            .into_iter()
            .filter(|s| seen.insert(s.name.clone()))
            .take(24)
            .map(|s| format!("`{}` ({})", s.name, s.rel))
            .collect();

        overview::Inputs {
            project_name,
            structure,
            entry_points,
            key_types,
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
        let mut by_file: Vec<&PathBuf> = self.symbol_index_by_file.keys().collect();
        by_file.sort_by_key(|p| self.rel_of(p));
        for abs in by_file {
            let names: Vec<&str> = self.symbol_index_by_file[abs]
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
        self.project
            .as_ref()?
            .files
            .iter()
            .find(|f| self.rel_of(&f.abs) == rel)
            .map(|f| f.abs.clone())
    }

    /// Navigate to walkthrough step `i`: open its file and jump to the symbol
    /// (resolved live against the index) or its fallback line.
    pub(crate) fn walkthrough_goto(&mut self, i: usize) -> Task<Message> {
        let Some(step) = self
            .walk
            .open
            .and_then(|o| self.walk.library.get(o))
            .and_then(|w| w.steps.get(i))
            .cloned()
        else {
            return Task::none();
        };
        self.walk.step = i;
        // Prepare the narration (markdown + any mermaid/math → SVG).
        let (prepared, render) = self.prepare_segments(&step.narration);
        self.walk.prepared = prepared;
        let Some(abs) = self.resolve_walk_file(&step.file) else {
            return render;
        };
        let line = step
            .symbol
            .as_ref()
            .and_then(|name| {
                self.symbol_index_by_file
                    .get(&abs)
                    .and_then(|syms| syms.iter().find(|s| &s.name == name))
                    .map(|s| s.line)
            })
            .or(step.line)
            .unwrap_or(1);
        Task::batch([self.open_file(abs, Some(line), true), render])
    }

    /// The `(node, text-to-embed, hash)` set for the semantic index: every
    /// explained function/file, embedding its `name/path — summary` (folders are
    /// too coarse to be useful search hits).
    pub(crate) fn gather_embed_nodes(&self) -> Vec<(explain::Node, String, incremental::Version)> {
        self.explain
            .cache
            .iter()
            .filter_map(|(node, cached)| {
                let text = match node {
                    explain::Node::Function { file, name } => {
                        format!("{name} in {} — {}", self.rel_of(file), cached.summary)
                    }
                    explain::Node::File(p) => format!("{} — {}", self.rel_of(p), cached.summary),
                    explain::Node::Folder(_) => return None,
                };
                let hash = embed::text_hash(&text);
                Some((node.clone(), text, hash))
            })
            .collect()
    }

    /// Build the answer context for an Ask question: each retrieved node's
    /// summary and (for functions) its source, capped in total size.
    pub(crate) fn gather_ask_context(&self, nodes: &[explain::Node]) -> String {
        const CAP: usize = 18000;
        let empty: HashMap<String, Option<String>> = HashMap::new();
        let mut ctx = String::new();
        for node in nodes {
            if ctx.len() >= CAP {
                break;
            }
            match node {
                explain::Node::Function { file, name } => {
                    let summary = self
                        .explain
                        .cache
                        .get(node)
                        .map(|c| c.summary.as_str())
                        .unwrap_or("");
                    let body = gather_fn_detail_input(file.clone(), name, &empty)
                        .map(|(_, body, _)| body)
                        .unwrap_or_default();
                    // Include the line so the model can cite an accurate jump anchor.
                    let rel = self.rel_of(file);
                    let loc = match self
                        .symbol_index_by_file
                        .get(file)
                        .and_then(|syms| syms.iter().find(|s| &s.name == name))
                        .map(|s| s.line)
                    {
                        Some(line) => format!("{rel} (L{line})"),
                        None => rel,
                    };
                    ctx.push_str(&format!(
                        "### {name} — {loc}\n{summary}\n```\n{body}\n```\n\n"
                    ));
                }
                explain::Node::File(p) => {
                    let summary = self
                        .explain
                        .cache
                        .get(node)
                        .map(|c| c.summary.as_str())
                        .unwrap_or("");
                    ctx.push_str(&format!("### {} (file)\n{summary}\n\n", self.rel_of(p)));
                }
                explain::Node::Folder(_) => {}
            }
        }
        ctx
    }

    /// Cosine similarity of a node's indexed embedding to the query vector, or 0
    /// when the node isn't in the index (e.g. a cursor anchor not yet embedded).
    pub(crate) fn node_score(&self, node: &explain::Node, qvec: &[f32]) -> f32 {
        self.embed_index
            .entries
            .iter()
            .find(|e| &e.node == node)
            .map(|e| embed::cosine(qvec, &e.vec))
            .unwrap_or(0.0)
    }

    /// Capture a pane's current text selection as a pinnable Ask context block.
    pub(crate) fn selection_pin(&self, pane: usize) -> Option<AskPin> {
        let v = self.panes.get(pane).and_then(Option::as_ref)?;
        let code = v.selected_text()?;
        let ((start_line, _), _) = v.selection_ordered()?;
        Some(AskPin {
            rel: v.rel.clone(),
            file: v.abs.clone(),
            line: start_line + 1, // 0-based → 1-based
            code,
        })
    }

    /// Resolve an overview markdown link (a project-relative path, optionally with
    /// a `#Lnn` line suffix) to an absolute file + line. Falls back to matching by
    /// file name when the exact path doesn't exist.
    pub(crate) fn resolve_project_link(&self, url: &str) -> Option<(PathBuf, Option<usize>)> {
        let project = self.project.as_ref()?;
        let (path_part, frag) = match url.rsplit_once('#') {
            Some((p, frag)) => (p.trim(), Some(frag.trim())),
            None => (url.trim(), None),
        };
        if path_part.is_empty() {
            return None;
        }
        let candidate = project.root.join(path_part);
        let abs = if candidate.is_file() {
            candidate
        } else {
            let base = std::path::Path::new(path_part).file_name()?;
            project
                .files
                .iter()
                .find(|f| f.abs.file_name() == Some(base))?
                .abs
                .clone()
        };
        // The fragment is a line number (`L68` / `68`), or a symbol name we
        // resolve to its line against the file's index (`#recompute`).
        let line = frag.and_then(|f| {
            f.trim_start_matches(['L', 'l'])
                .parse::<usize>()
                .ok()
                .or_else(|| {
                    self.symbol_index_by_file
                        .get(&abs)
                        .and_then(|syms| syms.iter().find(|s| s.name == f).map(|s| s.line))
                })
        });
        Some((abs, line))
    }
}
