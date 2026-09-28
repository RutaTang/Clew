//! The two project "homes": the architecture overview (generation, freshness,
//! the native module map) and the code statistics.
//!
//! Its messages, [`OverviewMsg`], arrive through `App::update_overview`.

use crate::app::prelude::*;
use crate::*;

/// The "this is not a real registry revision" stamp for `stats.rev`. The same
/// value `on_scan_done` writes on project load to force one recompute: the
/// freshness test is `stats.rev == registry.revision()`, and the revision
/// counter starts at 0 and only climbs, so this can never look fresh.
pub(crate) const STATS_REV_STALE: u64 = u64::MAX;

impl App {
    /// Whether the overview's inputs changed since it was generated, so a chained
    /// refresh regenerates it only when the result would actually differ (an
    /// overview pass is a full LLM call, unlike the incremental explain/index).
    pub(crate) fn overview_inputs_changed(&self) -> bool {
        let hash =
            incremental::content_hash(overview::prompt(&self.gather_overview_inputs()).as_bytes());
        self.proj.overview.prompt_hash != Some(hash)
    }

    /// The overview prose for display: strip any legacy mermaid "Module map"
    /// section a cached overview may still carry (the map is drawn natively now).
    pub(crate) fn overview_display(&self, raw: &str) -> String {
        overview::strip_module_map(raw)
    }

    /// Lay out the module map from the current import graph, or None when there's
    /// too little structure to show.
    pub(crate) fn compute_overview_map(&self) -> Option<graphlayout::Layout> {
        let (nodes, edges) = overview::module_layout_inputs(&self.proj.import_graph.scope_map())?;
        Some(graphlayout::layout(nodes, edges))
    }

    /// Recompute the native module-map layout, e.g. once the import graph finishes
    /// resolving. Cheap and synchronous — the map is a canvas, not a prose segment.
    pub(crate) fn refresh_overview_map(&mut self) -> Task<Message> {
        if self.proj.overview.markdown.is_some() {
            self.set_overview_map();
        }
        Task::none()
    }

    /// Lay out the overview's module map from the current import graph.
    pub(crate) fn set_overview_map(&mut self) {
        self.proj.overview.map = self.compute_overview_map();
        self.proj.overview.map_rev = ui::next_layout_rev();
    }

    /// Assemble the overview prompt inputs from clew's existing artifacts:
    /// folder/file summaries (the explanation cache), entry points and key types
    /// (the symbol index), and a computed module-dependency diagram (imports).
    pub(crate) fn gather_overview_inputs(&self) -> overview::Inputs {
        let root = self
            .proj
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
        for (node, cached) in &self.proj.explain.cache {
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
            .proj
            .symbol_index_by_file
            .values()
            .flat_map(|syms| syms.iter())
            .filter(|s| s.kind == "function" && s.name == "main")
            .map(|s| format!("`fn main` in {}", s.rel))
            .collect();
        entry_points.sort();
        entry_points.dedup();

        // Key types: struct/enum/class/trait symbols (capped, deterministic).
        let mut all_types: Vec<&SymbolEntry> = self
            .proj
            .symbol_index_by_file
            .values()
            .flat_map(|syms| syms.iter())
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

    pub(crate) fn on_generate_overview(&mut self) -> Task<Message> {
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        if self.proj.explain.cache.is_empty() {
            self.status =
                "Run Explain All first — the overview is built from the explanations".into();
            return Task::none();
        }
        if self.proj.project.is_none() {
            return Task::none();
        }
        let inputs = self.gather_overview_inputs();
        let prompt = overview::prompt(&inputs);
        let prompt_hash = incremental::content_hash(prompt.as_bytes());
        self.proj.overview.generating = true;
        // This request supersedes any still in flight (see `OverviewMsg::Done`).
        self.proj.overview.seq += 1;
        let seq = self.proj.overview.seq;
        // Don't force the overview into view: a chained/background
        // regeneration must not interrupt someone reading code. The
        // manual entry points are already on the overview page.
        self.status = "Generating architecture overview…".into();
        let ai = self.ai_client();
        let stamp = self.stamp();
        Task::perform(
            // Raw LLM prose only; the module map is folded in fresh at
            // prepare time so it always reflects the live imports.
            async move { ai.complete(cfg, overview::SYSTEM, prompt, 2048).await },
            move |result| {
                Message::Overview(OverviewMsg::Done {
                    stamp: stamp.clone(),
                    seq,
                    prompt_hash,
                    result,
                })
            },
        )
    }

    /// (Project ownership is checked in `dispatch`, from the stamp.)
    pub(crate) fn on_overview_done(
        &mut self,
        prompt_hash: incremental::Version,
        result: Result<String, String>,
    ) -> Task<Message> {
        self.proj.overview.generating = false;
        match result {
            Ok(markdown) => {
                // Persist the raw prose; fold the live module map in only
                // for display so the cache never carries a stale diagram.
                let saved = match &self.proj.derived_dir {
                    Some(store) => overview::save(
                        store,
                        &overview::Cached {
                            markdown: markdown.clone(),
                            prompt_hash,
                        },
                    ),
                    None => Ok(()),
                };
                let display = self.overview_display(&markdown);
                let (prepared, task) = self.prepare_segments(&display);
                self.proj.overview.prepared = prepared;
                self.proj.overview.markdown = Some(markdown);
                self.set_overview_map();
                self.proj.overview.prompt_hash = Some(prompt_hash);
                self.status = match saved {
                    Ok(()) => "Architecture overview ready".into(),
                    Err(e) => format!("Architecture overview ready — not cached: {e}"),
                };
                task
            }
            Err(e) => {
                self.status = format!("Overview failed: {e}");
                Task::none()
            }
        }
    }

    /// Kick off a stats computation off the UI thread when it's stale (or
    /// `force`d). Single-flight: never launches a second run while one is in
    /// flight. Stamps `stats_rev` with the registry revision so a later file
    /// change (which bumps the revision) marks the result stale.
    pub(crate) fn start_stats(&mut self, force: bool) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let rev = self.proj.registry.revision();
        let fresh = self.proj.stats.report.is_some() && self.proj.stats.rev == rev;
        if self.proj.stats.building || (!force && fresh) {
            return Task::none();
        }
        self.proj.stats.building = true;
        self.proj.stats.rev = rev;
        let stamp = self.stamp();
        if self.proj.stats.report.is_none() {
            self.status = "Computing code statistics…".into();
        }
        // Remote project: the walk happens where the files live — this
        // machine's disk at the same path is another project's data.
        if !self.local_project_state() {
            let ai = self.ai_client();
            return Task::perform(
                async move {
                    // A dead transport, the RPC timeout, a refusal: none of
                    // them is an answer, and each is reported as what it is —
                    // turning them into a report of zero files is what
                    // claimed a repo full of code had none.
                    match ai.request(clew_protocol::Request::Stats).await? {
                        clew_protocol::Event::Stats { report, .. } => Ok(report),
                        other => Err(format!(
                            "unexpected reply to Stats: {}",
                            crate::app::rpc::event_name(&other)
                        )),
                    }
                },
                move |report| {
                    Message::Overview(OverviewMsg::StatsDone {
                        stamp: stamp.clone(),
                        rev,
                        report,
                    })
                },
            );
        }
        let compute_root = root;
        Task::perform(
            // A panicked or cancelled compute is not an empty project either.
            async move {
                tokio::task::spawn_blocking(move || stats::compute(&compute_root))
                    .await
                    .map_err(|_| "the statistics run failed unexpectedly".to_string())
            },
            move |report| {
                Message::Overview(OverviewMsg::StatsDone {
                    stamp: stamp.clone(),
                    rev,
                    report,
                })
            },
        )
    }

    /// (Project ownership is checked in `dispatch`, from the stamp.)
    pub(crate) fn on_stats_done(
        &mut self,
        rev: u64,
        report: Result<stats::StatsReport, String>,
    ) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        // The run FAILED. Commit nothing: any report already shown stays, the
        // derived cache keeps whatever it had, and stamping `rev` stale is
        // what makes the next entry into the view retry instead of trusting
        // an answer that never came. Handing the failure over as an ordinary
        // empty report instead made the Stats view claim "No code files to
        // count in this project." for a repo full of code, announce "Code
        // statistics ready", cache the empty report, and stamp it fresh — so
        // re-entering the view never retried.
        let report = match report {
            Ok(report) => report,
            Err(e) => {
                self.proj.stats.building = false;
                self.proj.stats.rev = crate::app::overview::STATS_REV_STALE;
                self.status = format!("Couldn't compute code statistics: {e}");
                return Task::none();
            }
        };
        self.proj.stats.building = false;
        self.proj.stats.rev = rev;
        let saved = match &self.proj.derived_dir {
            Some(store) => stats::save(
                store,
                &stats::Cached {
                    report: report.clone(),
                    rev,
                },
            ),
            None => Ok(()),
        };
        self.proj.stats.report = Some(report);
        // A cache write that failed costs the next launch a recompute; say so
        // rather than claim the report was kept.
        self.status = match saved {
            Ok(()) => "Code statistics ready".into(),
            Err(e) => format!("Code statistics ready — not cached: {e}"),
        };
        Task::none()
    }
}

impl App {
    /// Handle a [`OverviewMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_overview(&mut self, message: OverviewMsg) -> Task<Message> {
        match message {
            OverviewMsg::Show => {
                self.proj.overview.showing = true;
                self.proj.stats.showing = false;
                self.proj.docs.page = None;
                Task::none()
            }
            OverviewMsg::ShowStats => {
                self.proj.stats.showing = true;
                self.proj.overview.showing = false;
                self.proj.docs.page = None;
                // Compute on entry when there's nothing to show or the file set
                // changed since the last run; otherwise the cached report stays.
                self.start_stats(false)
            }
            OverviewMsg::RefreshStats => self.start_stats(true),
            OverviewMsg::StatsDone { rev, report, .. } => self.on_stats_done(rev, report),
            OverviewMsg::Generate => self.on_generate_overview(),
            OverviewMsg::Done {
                seq,
                prompt_hash,
                result,
                ..
            } => {
                // Supersession is decided BEFORE any flag is touched: an older
                // generation landing after a newer one started must neither
                // install its prose nor clear the newer one's busy flag.
                if seq != self.proj.overview.seq {
                    return Task::none();
                }
                self.on_overview_done(prompt_hash, result)
            }
        }
    }
}
