//! Rich content and the reading context: preparing LLM markdown for display
//! (math and mermaid rendered to SVG, citation links), the session SVG cache,
//! opening links, and keeping the side panels in step with the caret.
//!
//! Its messages, [`ContentMsg`], arrive through `App::update_content`.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// The project's rel-path lookup tables (see [`CitationIndex`]), built
    /// once per file list — a project open or a rescan installs a new one,
    /// and identity is the freshness test — rather than once per use: the
    /// callers run per caret move or per watcher publication, over up to a
    /// hundred thousand files.
    pub(crate) fn fresh_citation_index(&mut self) -> Option<&CitationIndex> {
        let files = self.proj.project.as_ref()?.files.clone();
        let stale = self
            .proj
            .citation_index
            .as_ref()
            .is_none_or(|idx| !Arc::ptr_eq(&idx.files, &files));
        if stale {
            self.proj.citation_index = Some(CitationIndex::build(files));
        }
        self.proj.citation_index.as_ref()
    }

    /// Segment `content` (LLM markdown) for display: parse markdown, key the
    /// math/mermaid, load cached SVGs, and return a background task to render the
    /// rest. Shared by the explanation panel and the architecture overview.
    pub(crate) fn prepare_segments(&mut self, content: &str) -> (Vec<PreparedSeg>, Task<Message>) {
        // `path:line` citations become clickable jumps — but only for paths
        // that resolve to a real project file (an exact rel, or a bare file
        // name that is unique in the project), so type names stay code chips.
        //
        // The two lookup tables are built once per file list (a project open
        // or a rescan installs a new one) and kept: this runs on every caret
        // move that changes the explained symbol, and rebuilding two maps over
        // up to a hundred thousand files each time was pure waste.
        let content = match self.fresh_citation_index() {
            Some(idx) => richmd::linkify_citations(content, |path| idx.resolve(path)),
            None => content.to_string(),
        };
        let segments = richmd::segment(&content);
        let root = self.proj.project.as_ref().map(|p| p.root.clone());

        // Pull cached SVGs into memory; collect what still needs rendering. A
        // source the renderer already failed on this session is not tried
        // again (it shows as source; the next session retries it).
        let mut missing: Vec<richmd::Renderable> = Vec::new();
        for r in richmd::renderables(&segments) {
            if self.proj.explain.svgs.contains_key(&r.key)
                || self.proj.explain.svg_failed.contains_key(&r.key)
            {
                continue;
            }
            let cached = self
                .proj
                .derived_dir
                .as_deref()
                .and_then(|store| richmd::load_raw(store, r.key));
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
                    PreparedSeg::DisplayMath(richmd::math_key(&tex, true), tex)
                }
                richmd::Segment::Mermaid(src) => {
                    PreparedSeg::Mermaid(richmd::mermaid_key(&src), src)
                }
                richmd::Segment::Code { lang, code } => {
                    // Same tree-sitter pipeline (and palette) as the editor;
                    // unknown languages fall back to plain monospace lines.
                    let lines = highlight::highlight_lines(&code, highlight::lang_for_fence(&lang));
                    PreparedSeg::Code(lines)
                }
                richmd::Segment::InlineLine(parts) => PreparedSeg::InlineLine(
                    parts
                        .into_iter()
                        .map(|p| match p {
                            richmd::Inline::Text(t) => {
                                PreparedInline::Text(richmd::InlinePiece::parse(&t))
                            }
                            richmd::Inline::Math(tex) => {
                                PreparedInline::Math(richmd::math_key(&tex, false), tex)
                            }
                        })
                        .collect(),
                ),
            })
            .collect();

        // Render any missing diagrams/equations in the background.
        let store = self.proj.derived_dir.clone();
        let task = match root {
            Some(_) if !missing.is_empty() => {
                self.proj.explain.svg_gen += 1;
                let generation = self.proj.explain.svg_gen;
                self.status = "Rendering math & diagrams…".into();
                let stamp = self.stamp();
                // Kept to report the batch if its task itself fails: an empty
                // batch (`unwrap_or_default`) read as "Rendered math &
                // diagrams" while none were, and nothing marked them failed,
                // so the next draw ran the same renders again.
                let keys: Vec<u64> = missing.iter().map(|r| r.key).collect();
                Task::perform(
                    run_render_batch(keys, move || generate_svgs(missing, store)),
                    move |map| {
                        Message::Content(ContentMsg::SvgsGenerated {
                            stamp: stamp.clone(),
                            generation,
                            map,
                        })
                    },
                )
            }
            _ => Task::none(),
        };
        (prepared, task)
    }

    /// Insert a prepared SVG into the session cache, building its iced handle.
    pub(crate) fn insert_svg(&mut self, key: u64, prepared: richmd::PreparedSvg) {
        self.proj.explain.svgs.insert(
            key,
            ExplainSvg {
                handle: iced::widget::svg::Handle::from_memory(prepared.svg.into_bytes()),
                width: prepared.width,
                height: prepared.height,
            },
        );
    }

    /// Re-color every cached math/mermaid SVG for the current theme. Their raw
    /// sources are theme-independent (math paints `currentColor`, mermaid keeps
    /// its slate palette), so reloading and re-preparing each is enough to make
    /// an already-open explanation follow a light/dark switch.
    pub(crate) fn restyle_svgs(&mut self) {
        // No store means the SVGs were only ever in memory; they keep their
        // colors until re-rendered.
        let Some(store) = self.proj.derived_dir.clone() else {
            return;
        };
        let keys: Vec<u64> = self.proj.explain.svgs.keys().copied().collect();
        for key in keys {
            if let Some(raw) = richmd::load_raw(&store, key) {
                // The renderer stamps its kind on the SVG's root element.
                let is_math = richmd::svg_kind(&raw).is_math();
                self.insert_svg(key, richmd::prepare_svg(&raw, is_math));
            }
        }
    }

    pub(crate) fn on_svgs_generated(&mut self, generation: u64, batch: SvgBatch) -> Task<Message> {
        // SVGs are keyed by content hash (and disk-cached), so inserting
        // is idempotent — accept them even from a superseded generation,
        // otherwise a concurrent `prepare_segments` bumping the counter
        // can strand a diagram as a perpetual placeholder.
        let rendered = batch.rendered.len();
        for (key, prepared) in batch.rendered {
            self.insert_svg(key, prepared);
        }
        // A failure is remembered (the view shows the source in its place),
        // so `prepare_segments` does not re-run a renderer that already failed
        // (or panicked) on this source every time the panel is drawn.
        let failed = batch.failed.len();
        let first_reason = batch.failed.first().map(|f| f.reason.clone());
        for failure in batch.failed {
            self.proj
                .explain
                .svg_failed
                .insert(failure.key, failure.reason);
        }
        if generation == self.proj.explain.svg_gen {
            self.status = match first_reason {
                None => "Rendered math & diagrams".into(),
                Some(reason) if rendered == 0 => {
                    format!("Could not render math & diagrams: all {failed} failed ({reason})")
                }
                Some(reason) => format!("Rendered {rendered} · {failed} failed ({reason})"),
            };
        }
        Task::none()
    }

    pub(crate) fn on_open_link(&mut self, url: String) -> Task<Message> {
        // clew:<rel>[:line] — the citation scheme `linkify_citations` mints;
        // jump to that file (and line) in the editor. Provenance is NOT
        // guaranteed, see the check below.
        if let Some(target) = url.strip_prefix("clew:") {
            let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            let (rel, line) = match target.rsplit_once(':') {
                Some((p, n)) => match n.parse::<usize>() {
                    Ok(n) => (p, Some(n)),
                    Err(_) => (target, None),
                },
                None => (target, None),
            };
            // The scheme is only ever MINTED by `linkify_citations`, but nothing
            // forces a `clew:` URL to have come from there: iced's markdown
            // widget passes any link destination through verbatim, and the
            // markdown we render is untrusted — a repository README, a `///`
            // doc comment shown in the Docs tab, a prompt-injected LLM answer.
            // So `rel` is attacker data. `Path::join` DISCARDS the root when the
            // argument is absolute, so `[Setup](clew:/Users/me/.ssh/id_rsa)`
            // used to open that file in the editor under a project-looking
            // breadcrumb, from one click on innocuous link text.
            //
            // This test is purely lexical, so it holds identically for a REMOTE
            // project, where probing this machine's disk at the remote's paths
            // is itself forbidden. It rejects exactly what `linkify_citations`
            // never emits: empty, absolute, and any `.`/`..` component.
            if !clew_core::statefile::safe_rel(rel) {
                self.status = format!("Refused a link outside the project: {url}");
                return Task::none();
            }
            let abs = root.join(rel);
            // Local only, and only defence in depth: a repo-shipped symlinked
            // DIRECTORY component (`link/x` with `link -> /etc`) passes every
            // lexical test, and the fallback read used when clew-server isn't
            // up (`tasks::read_text_file`) has no containment check of its own.
            // A remote target must never be resolved against this filesystem,
            // so remote keeps the lexical result alone and relies on the
            // server's own `confine` in `ReadFile`. Decided from the path, and
            // the read that follows resolves it again, so a component swapped
            // in between stays open — the same residual `.clew` documents.
            if self.local_project_state() && !clew_core::fs_scan::is_inside(&root, &abs) {
                self.status = format!("Couldn't open {rel}: not a file inside the project");
                return Task::none();
            }
            return self.open_file(abs, line, true);
        }
        // http(s): hand a validated plain URL to the OS opener — never
        // file://, javascript:, a leading '-' (flag injection), etc.
        if url.starts_with("http://") || url.starts_with("https://") {
            let safe = !url.contains(['\n', '\r', '\0']) && url.len() < 2048;
            if safe {
                // By absolute path where the system ships one: a bare name is
                // resolved through PATH, which may name a directory the user
                // (or a project's tooling) can write to.
                let opener = if cfg!(target_os = "macos") {
                    "/usr/bin/open"
                } else if cfg!(target_os = "windows") {
                    "explorer"
                } else {
                    "xdg-open"
                };
                // The opener not starting is a failure to report, not a no-op:
                // the click otherwise looks broken with no explanation.
                let mut open = std::process::Command::new(opener);
                open.arg(&url).stdin(std::process::Stdio::null());
                if let Err(e) = spawn_reaped(open) {
                    self.status = format!("Couldn't open the link: {e}");
                }
            } else {
                self.status = format!("Refused to open link: {url}");
            }
            return Task::none();
        }
        // Otherwise treat it as a project-file reference (the overview's
        // links), e.g. `src/find.rs` or `find.rs#L20` — jump to it.
        if let Some((abs, line)) = self.resolve_project_link(&url) {
            self.proj.overview.showing = false;
            self.proj.stats.showing = false;
            return self.open_file(abs, line, true);
        }
        self.status = format!("Couldn't resolve link: {url}");
        Task::none()
    }

    /// Resolve an overview markdown link (a project-relative path, optionally with
    /// a `#Lnn` line suffix) to an absolute file + line. Falls back to matching by
    /// file name when the exact path doesn't exist.
    pub(crate) fn resolve_project_link(&self, url: &str) -> Option<(PathBuf, Option<usize>)> {
        let project = self.proj.project.as_ref()?;
        let (path_part, frag) = match url.rsplit_once('#') {
            Some((p, frag)) => (p.trim(), Some(frag.trim())),
            None => (url.trim(), None),
        };
        // The link text comes from an LLM answer or the repository's own
        // markdown, so it is untrusted: `../../etc/passwd` must not resolve,
        // and for a REMOTE project nothing here may probe this machine's disk
        // at the remote's paths.
        let path_part = path_part.trim_start_matches("./");
        if path_part.is_empty() || !clew_core::statefile::safe_rel(path_part) {
            return None;
        }
        let candidate = project.root.join(path_part);
        // Match against the project's own file list first — that works
        // identically for a local and a remote project. A disk probe is only
        // meaningful, and only allowed, for a local one.
        let listed = project.files.iter().any(|f| f.abs == candidate);
        let abs = if listed || (self.local_project_state() && candidate.is_file()) {
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
                    self.proj
                        .symbol_index_by_file
                        .get(&abs)
                        .and_then(|syms| syms.iter().find(|s| s.name == f).map(|s| s.line))
                })
        });
        Some((abs, line))
    }

    /// Point the explanation panel at the function/file under the caret. No-op if
    /// it already shows that target (so moving within one function is free).
    /// `extra` is the caller's own task (e.g. a scroll), run alongside.
    pub(crate) fn follow_caret(&mut self, extra: Task<Message>) -> Task<Message> {
        let Some(target) = self.cursor_target() else {
            return extra;
        };
        if self.proj.explain.view.as_ref() == Some(&target) {
            return extra;
        }
        Task::batch([extra, self.show_explanation(target)])
    }

    /// Follow the reading cursor: keep the context panel showing the function
    /// (or, between functions, the file) the caret is in. A cheap no-op when the
    /// panel is closed or the enclosing symbol hasn't changed, so it is safe to
    /// call on every caret move. Never opens the panel on its own — that stays a
    /// deliberate act (toggle, or Cmd+click to explain).
    pub(crate) fn sync_reading_context(&mut self) -> Task<Message> {
        if !self.show_right_panel || self.proj.split {
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
                ordinal: outline::fn_ordinal(&v.symbols, s),
            })
            .unwrap_or(explain::Node::File(abs));
        if self.proj.explain.view.as_ref() == Some(&target) {
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
        let (name, ordinal) = match &self.proj.explain.view {
            Some(explain::Node::Function {
                file,
                name,
                ordinal,
            }) if *file == v.abs => (name.clone(), *ordinal),
            _ => return Task::none(),
        };
        let mut y = 0.0f32;
        let mut found = false;
        // One pass for every symbol's same-name ordinal: `fn_ordinal` per
        // symbol inside this loop scanned the whole list per symbol —
        // quadratic, on every caret move.
        let ordinals = outline::fn_ordinals(&v.symbols);
        for (s, &symbol_ordinal) in v.symbols.iter().zip(&ordinals) {
            if matches!(s.kind.as_str(), "function" | "method")
                && s.name == name
                && symbol_ordinal == ordinal
            {
                found = true;
                break;
            }
            // Mirror ui::outline_content's row layout: a label line, plus a summary
            // line when inline summaries are on and this symbol has a real one.
            let mut h = 27.0;
            let has_summary = self.show_inline_summaries
                && matches!(s.kind.as_str(), "function" | "method")
                && self
                    .proj
                    .explain
                    .cache
                    .get(&explain::Node::Function {
                        file: v.abs.clone(),
                        name: s.name.clone(),
                        ordinal: symbol_ordinal,
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
}

impl App {
    /// Handle a [`ContentMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_content(&mut self, message: ContentMsg) -> Task<Message> {
        match message {
            ContentMsg::SvgsGenerated {
                generation, map, ..
            } => self.on_svgs_generated(generation, map),
            ContentMsg::OpenLink(url) => self.on_open_link(url),
        }
    }
}

/// Start `cmd` without waiting for it, and reap it when it exits (on a
/// thread of its own), returning its pid. A child that is spawned and dropped
/// is never waited for, and every link click left an exited `open` behind as
/// a zombie for the rest of the session.
pub(crate) fn spawn_reaped(mut cmd: std::process::Command) -> std::io::Result<u32> {
    let mut child = cmd.spawn()?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// Run a render batch on the blocking pool. When the batch's task itself fails
/// (it panicked), EVERY item it was rendering is reported failed, with why —
/// `keys` are those items. An empty batch in its place read as "Rendered math
/// & diagrams" while none were, and nothing marked them failed, so the next
/// draw ran the same renders again.
pub(crate) async fn run_render_batch(
    keys: Vec<u64>,
    render: impl FnOnce() -> SvgBatch + Send + 'static,
) -> SvgBatch {
    tokio::task::spawn_blocking(render)
        .await
        .unwrap_or_else(|e| SvgBatch {
            rendered: HashMap::new(),
            failed: keys
                .into_iter()
                .map(|key| SvgFailure {
                    key,
                    reason: format!("the render task failed: {e}"),
                })
                .collect(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A19: a program started for a link click is reaped once it exits, not
    /// left a zombie: its pid stops naming a process at all.
    #[test]
    fn a_started_opener_is_reaped_when_it_exits() {
        let pid = spawn_reaped(std::process::Command::new("/usr/bin/true")).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // SAFETY: kill(2) with signal 0 only checks that the pid exists.
        while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the exited opener is still a process: a zombie nobody reaps"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// B-findings #8, through the real mapping: a render task that panics
    /// comes back as a batch in which every item it was rendering failed —
    /// saying so — not as an empty batch that reads as success.
    #[tokio::test]
    async fn a_render_task_that_panics_fails_every_item_it_held() {
        let batch = run_render_batch(vec![3, 5], || panic!("the renderer crashed")).await;
        assert!(batch.rendered.is_empty());
        let failed: Vec<(u64, bool)> = batch
            .failed
            .iter()
            .map(|f| (f.key, f.reason.starts_with("the render task failed")))
            .collect();
        assert_eq!(failed, [(3, true), (5, true)]);

        // A batch that ran is passed through as it came.
        let fine = run_render_batch(vec![7], SvgBatch::default).await;
        assert!(fine.rendered.is_empty() && fine.failed.is_empty());
    }
}
