//! The two project "homes": the architecture overview (generation, freshness,
//! the architecture diagram) and the code statistics.
//!
//! Its messages, [`OverviewMsg`], arrive through `App::update_overview`.

use crate::app::prelude::*;
use crate::*;

/// The "this is not a real registry revision" stamp for `stats.rev`. The same
/// value `on_scan_done` writes on project load to force one recompute: the
/// freshness test is `stats.rev == registry.revision()`, and the revision
/// counter starts at 0 and only climbs, so this can never look fresh.
pub(crate) const STATS_REV_STALE: u64 = u64::MAX;

/// How many entry points the overview prompt names (mains first, then
/// routes, commands and handlers); the rest are counted, not listed.
pub(crate) const ENTRY_POINTS_SHOWN: usize = 48;

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

    /// The project's architecture, read from its files, its import graph,
    /// its manifests and doc comments (a local project's: a remote one's
    /// files are not here to read) and the AI's summaries of its folders;
    /// `None` with no project open.
    pub(crate) fn compute_architecture(&self) -> Option<crate::architecture::Architecture> {
        let project = self.proj.project.as_ref()?;
        let root = project.root.clone();
        let graph = &self.proj.import_graph;
        let rel_of = |p: &Path| {
            p.strip_prefix(&root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
        };
        let mut imports: Vec<(String, String)> = Vec::new();
        let mut externals: Vec<(String, String)> = Vec::new();
        for f in project.files.iter() {
            for e in graph.imports(&f.abs) {
                if !e.kind.is_dependency() {
                    continue;
                }
                match &e.target {
                    imports::Target::Internal(to) => {
                        if let Some(to) = rel_of(to) {
                            imports.push((f.rel.clone(), to));
                        }
                    }
                    imports::Target::External(name) => {
                        externals.push((f.rel.clone(), name.clone()))
                    }
                    imports::Target::Unresolved(_) => {}
                }
            }
        }
        let summaries = self
            .proj
            .explain
            .cache
            .iter()
            .filter_map(|(node, cached)| match node {
                explain::Node::Folder(p) => rel_of(p).map(|rel| (rel, cached.summary.clone())),
                _ => None,
            })
            .collect();
        let mains = self
            .proj
            .symbol_index_by_file
            .values()
            .flat_map(|syms| syms.iter())
            .filter(|s| s.entry == Some(index::EntryKind::Main))
            .map(|s| s.rel.clone())
            .collect();
        let local = !self.connection.is_remote();
        let read = |rel: &str| -> Option<String> {
            local
                .then(|| {
                    clew_core::fs_scan::read_confined_capped(&root, &root.join(rel), 256 * 1024)
                })
                .flatten()
        };
        Some(crate::architecture::read(&crate::architecture::Inputs {
            files: project.files.iter().map(|f| f.rel.as_str()).collect(),
            imports,
            externals,
            read: &read,
            summaries,
            mains,
        }))
    }

    /// Read the architecture anew — the import graph resolved, the overview
    /// written — while the overview is shown; else let it go, to be read
    /// on Show: it reads manifests and doc comments, which a project being
    /// edited need not pay for at every save while nobody looks.
    pub(crate) fn refresh_architecture(&mut self) {
        if self.proj.overview.showing {
            self.set_architecture();
        } else if self.proj.overview.architecture.take().is_some() {
            self.proj.overview.arch_rev = ui::next_layout_rev();
        }
    }

    /// Read the overview's architecture.
    pub(crate) fn set_architecture(&mut self) {
        self.proj.overview.architecture = self.compute_architecture().map(Arc::new);
        self.proj.overview.arch_rev = ui::next_layout_rev();
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

        // Entry points: every function the index classifies as one — mains
        // first, then routes, commands and handlers (see
        // `clew_core::outline::entry_kind`) — capped, so a service with
        // hundreds of routes does not turn the prompt into its route table;
        // the cap says how many it left out.
        let mut entries: Vec<(index::EntryKind, &str, &str)> = self
            .proj
            .symbol_index_by_file
            .values()
            .flat_map(|syms| syms.iter())
            .filter_map(|s| s.entry.map(|kind| (kind, s.rel.as_str(), s.name.as_str())))
            .collect();
        entries.sort();
        entries.dedup();
        let mut entry_points: Vec<String> = entries
            .iter()
            .take(ENTRY_POINTS_SHOWN)
            .map(|(kind, rel, name)| match kind {
                index::EntryKind::Main => format!("`fn main` in {rel}"),
                kind => format!("{} `{name}` in {rel}", kind.label()),
            })
            .collect();
        if entries.len() > ENTRY_POINTS_SHOWN {
            entry_points.push(format!("… and {} more", entries.len() - ENTRY_POINTS_SHOWN));
        }

        // Key types: struct/enum/class/trait symbols (capped, deterministic).
        let mut all_types: Vec<&SymbolEntry> = self
            .proj
            .symbol_index_by_file
            .values()
            .flat_map(|syms| syms.iter())
            .filter(|s| {
                matches!(
                    s.kind.as_str(),
                    "struct" | "enum" | "class" | "trait" | "interface" | "table" | "inductive"
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
            self.status = tr!("Run Explain All first").into();
            return Task::none();
        }
        if self.proj.project.is_none() {
            return Task::none();
        }
        let inputs = self.gather_overview_inputs();
        let prompt = overview::prompt(&inputs);
        let prompt_hash = incremental::content_hash(prompt.as_bytes());
        self.proj.overview.generating = true;
        self.proj.overview.failed = None;
        // This request supersedes any still in flight (see `OverviewMsg::Done`).
        self.proj.overview.seq += 1;
        let seq = self.proj.overview.seq;
        // Don't force the overview into view: a chained/background
        // regeneration must not interrupt someone reading code. The
        // manual entry points are already on the overview page.
        self.status = tr!("Generating architecture overview…").into();
        let ai = self.ai_client();
        let stamp = self.stamp();
        Task::perform(
            // Raw LLM prose only; the module map is folded in fresh at
            // prepare time so it always reflects the live imports.
            async move {
                ai.complete(
                    cfg,
                    crate::app::usage::OVERVIEW,
                    overview::SYSTEM,
                    prompt,
                    2048,
                )
                .await
            },
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
                // Persist the raw prose; the architecture is drawn live beside
                // it, never cached with it.
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
                self.refresh_architecture();
                self.proj.overview.prompt_hash = Some(prompt_hash);
                self.status = match saved {
                    Ok(()) => tr!("Architecture overview ready").into(),
                    Err(e) => tr!(
                        "Architecture overview ready, but not cached: {error}",
                        error = e
                    ),
                };
                task
            }
            Err(e) => {
                self.status = tr!("Could not generate the overview: {error}", error = e);
                self.proj.overview.failed = Some(crate::app::tasks::ai_reason(&e));
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
            self.status = tr!("Computing code statistics…").into();
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
                    .map_err(|_| tr!("the statistics run failed unexpectedly").to_string())
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
                self.status = tr!("Couldn't compute code statistics: {error}", error = e);
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
            Ok(()) => tr!("Code statistics ready").into(),
            Err(e) => tr!("Code statistics ready, but not cached: {error}", error = e),
        };
        Task::none()
    }
}

impl App {
    /// Put away the page covering the code — a doc page, the overview, the
    /// statistics, the glossary, a plugin's — and say whether there was one: Esc, and
    /// Back over a file, go to the code under it first.
    pub(crate) fn put_away_page(&mut self) -> bool {
        let covered = self.proj.docs.page.is_some()
            || self.proj.overview.showing
            || self.proj.stats.showing
            || self.proj.weak.showing
            || self.proj.glossary.showing
            || self.proj.plugin_page.is_some();
        self.proj.docs.page = None;
        self.proj.plugin_page = None;
        self.proj.overview.showing = false;
        self.proj.stats.showing = false;
        self.proj.weak.showing = false;
        self.proj.glossary.showing = false;
        covered
    }

    /// Handle a [`OverviewMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_overview(&mut self, message: OverviewMsg) -> Task<Message> {
        match message {
            OverviewMsg::Show => {
                // The architecture, drawn from the imports, is there before
                // any prose is written about it.
                if self.proj.overview.architecture.is_none() {
                    self.set_architecture();
                }
                self.proj.overview.showing = true;
                self.proj.stats.showing = false;
                self.proj.weak.showing = false;
                self.proj.glossary.showing = false;
                self.proj.docs.page = None;
                self.proj.plugin_page = None;
                Task::none()
            }
            OverviewMsg::ShowStats => {
                self.proj.stats.showing = true;
                self.proj.overview.showing = false;
                self.proj.glossary.showing = false;
                self.proj.docs.page = None;
                self.proj.plugin_page = None;
                // Compute on entry when there's nothing to show or the file set
                // changed since the last run; otherwise the cached report stays.
                self.start_stats(false)
            }
            OverviewMsg::RefreshStats => self.start_stats(true),
            OverviewMsg::Close => {
                self.proj.overview.showing = false;
                self.proj.stats.showing = false;
                self.proj.weak.showing = false;
                Task::none()
            }
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
