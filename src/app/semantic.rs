//! Semantic search: building and merging the embedding index over the
//! explanation summaries, keeping it within one embedding space, and ranking a
//! query against it.
//!
//! Its messages, [`SemanticMsg`], arrive through `App::update_semantic`.

use crate::app::prelude::*;
use crate::*;

/// A finished build, kept only while the stored config still names the space
/// it was built in (`space`); else handed back unsaved, like any failed
/// build's index. The vectors are all of the one space the index is stamped
/// with, but the window may no longer search in it — the live-config check
/// decides at its next use (`App::drop_foreign_embed_index`).
pub(crate) fn built_in_stored_space(
    index: embed::Index,
    space: &embed::Space,
    stored: &embed::Space,
) -> Result<embed::Index, EmbedBuildFailed> {
    if stored != space {
        return Err(EmbedBuildFailed::new(
            "embedding config changed while the index was building — not saved, build it again",
            index,
        ));
    }
    Ok(index)
}

/// What a merge on the blocking pool comes back as: the merged index and
/// whether it was saved — or, the merge having panicked, the build itself,
/// unsaved. Its vectors were paid for; adopting an empty index threw them
/// away.
pub(crate) fn merged_or_built(
    built: Arc<embed::Index>,
    merged: Result<(embed::Index, Result<(), String>), tokio::task::JoinError>,
) -> (embed::Index, Result<(), String>) {
    merged.unwrap_or_else(|e| {
        let built = Arc::try_unwrap(built).unwrap_or_else(|a| (*a).clone());
        (built, Err(format!("the merge failed unexpectedly: {e}")))
    })
}

impl App {
    /// Retire a query before its index or embedding configuration changes.
    /// Dropping its guard also stops the request still running at the endpoint.
    pub(crate) fn retire_semantic_search(&mut self) {
        self.proj.semantic_seq += 1;
        self.proj.searching_semantic = false;
        self.proj.inflight.semantic_search = None;
    }

    /// Discard the in-memory index when `cfg` names an embedding space its
    /// vectors cannot belong to, so a query is never ranked in one space
    /// against vectors from another and a rebuild never reuses them.
    ///
    /// Called on every path that USES the index, because the config it was
    /// loaded under can move under a running session: `embed::load_for` applies
    /// the same rule to the file but only at project open, and
    /// `on_settings_saved` covers this window's own Settings. What is left for
    /// here is a change made by ANOTHER window or by hand-editing `config.toml`,
    /// of either half of the space: the index carries both the model and the
    /// endpoint its vectors came from ([`embed::Space::is_foreign`]). It is
    /// also what judges an index a failed build handed back
    /// (`on_embeddings_built`).
    pub(crate) fn drop_foreign_embed_index(&mut self, cfg: &embed::Config) {
        if cfg.space().is_foreign(&self.proj.embed_index) {
            self.retire_semantic_search();
            self.proj.embed_index = embed::Index::default();
            self.proj.semantic_results.clear();
        }
    }

    pub(crate) fn on_build_embeddings(&mut self) -> Task<Message> {
        // One build at a time. A running build holds the index it extends
        // (the slot is empty meanwhile), so a second found nothing to reuse
        // and embedded — and billed — every node again, and whichever landed
        // first cleared the flag the other still ran under. The request waits
        // and goes out once the running build lands, against its result.
        if self.proj.building_embeddings {
            self.proj.embed_build_pending = true;
            return Task::none();
        }
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        self.drop_foreign_embed_index(&cfg);
        if self.proj.explain.cache.is_empty() {
            self.status = "Run Explain All first — the index embeds the summaries".into();
            return Task::none();
        }
        if self.proj.project.is_none() {
            return Task::none();
        }
        let nodes = self.gather_embed_nodes();
        // A build takes the index out of this window until its merge lands.
        // A query arriving in that interval must not rank an empty placeholder.
        self.retire_semantic_search();
        // Moved into the build (it reuses these vectors rather than copying
        // them), and handed back whole on every way the build can fail —
        // `on_embeddings_built` puts it back.
        let existing = std::mem::take(&mut self.proj.embed_index);
        self.proj.building_embeddings = true;
        self.status = "Building semantic index…".into();
        let ai = self.ai_client();
        let stamp = self.stamp();
        // The space these vectors are being built in. The saved file is
        // stamped with the index's OWN space (`embed::save` / `merge_built`
        // write `index.model` / `index.base_url`), so a config change cannot
        // mislabel them there. It is not saved all the same when the config
        // changed while it was building: the vectors are of a space the
        // window may no longer search in. It is handed back like any failed
        // build's index — every vector in it is of the one space it is
        // stamped with — and the live-config check decides at its next use
        // (`drop_foreign_embed_index`).
        let space = cfg.space();
        let (build, handle) = Task::perform(
            async move {
                let index = build_embeddings(&ai, &cfg, nodes, existing).await?;
                built_in_stored_space(index, &space, &embed::stored_space())
            },
            move |result| {
                Message::Semantic(SemanticMsg::IndexBuilt {
                    stamp: stamp.clone(),
                    result,
                })
            },
        )
        .abortable();
        // Held by the project: leaving it aborts the build (`InFlight`).
        self.proj.inflight.embed_build = Some(handle.abort_on_drop());
        build
    }

    /// (Project ownership is checked in `dispatch`, from the stamp.)
    pub(crate) fn on_embeddings_built(
        &mut self,
        result: Result<embed::Index, EmbedBuildFailed>,
    ) -> Task<Message> {
        // The build's task is over: nothing left to abort.
        self.proj.inflight.embed_build = None;
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        match result {
            Ok(index) => {
                // Folded into the stored index rather than written over it:
                // this build covers the nodes THIS window's explanation cache
                // holds, and the derived store is shared by every window and
                // every clew process on the project, so a wholesale write
                // shrank a whole-project index down to one window's subset.
                //
                // The merge re-reads, merges and re-serializes the whole index
                // (vectors as JSON text: megabytes), so it runs on the blocking
                // pool; the build stays "in progress" until it lands.
                let Some(store) = self.proj.derived_dir.clone() else {
                    return self.on_embeddings_merged(Some(index), Ok(()));
                };
                let stamp = self.stamp();
                let task_root = root;
                Task::perform(
                    async move {
                        let built = Arc::new(index);
                        let merging = built.clone();
                        let merged = tokio::task::spawn_blocking(move || {
                            let (merged, saved) = embed::merge_built(&store, &task_root, &merging);
                            (merged, saved.map_err(|e| e.to_string()))
                        })
                        .await;
                        merged_or_built(built, merged)
                    },
                    move |(merged, saved)| {
                        Message::Semantic(SemanticMsg::IndexMerged {
                            stamp: stamp.clone(),
                            index: Handoff::new(merged),
                            saved,
                        })
                    },
                )
            }
            Err(failed) => {
                self.proj.building_embeddings = false;
                let waiting = self.waiting_index_build();
                self.status = format!("Index build failed: {}", failed.error);
                // The index the build started from comes back with it: the
                // window keeps what it had (nothing else fills the slot while
                // a build runs), and the next build reuses it instead of
                // embedding — and billing — every node again.
                if let Some(previous) = failed.previous.take()
                    && self.proj.embed_index.entries.is_empty()
                {
                    self.proj.embed_index = previous;
                }
                waiting
            }
        }
    }

    /// A finished build was merged into the stored index (see
    /// `on_embeddings_built`). The merged index is adopted whether or not the
    /// write landed: the vectors are usable for this session, and only the
    /// persistence failed — which the status line says.
    pub(crate) fn on_embeddings_merged(
        &mut self,
        index: Option<embed::Index>,
        saved: Result<(), String>,
    ) -> Task<Message> {
        self.proj.building_embeddings = false;
        let waiting = self.waiting_index_build();
        let Some(index) = index else {
            return waiting;
        };
        self.status = match saved {
            Ok(()) => format!("Semantic index ready ({} items)", index.entries.len()),
            Err(e) => format!(
                "Semantic index ready ({} items) — not saved: {e}",
                index.entries.len()
            ),
        };
        self.proj.embed_index = index;
        waiting
    }

    /// The build asked for while the one that just landed ran, if any.
    fn waiting_index_build(&mut self) -> Task<Message> {
        if std::mem::take(&mut self.proj.embed_build_pending) {
            Task::done(Message::Semantic(SemanticMsg::BuildIndex))
        } else {
            Task::none()
        }
    }

    /// The `(node, text-to-embed, hash)` set for the semantic index: every
    /// explained function/file, embedding its `name/path — summary` (folders are
    /// too coarse to be useful search hits).
    pub(crate) fn gather_embed_nodes(&self) -> Vec<(explain::Node, String, incremental::Version)> {
        self.proj
            .explain
            .cache
            .iter()
            .filter_map(|(node, cached)| {
                let text = match node {
                    explain::Node::Function { file, name, .. } => {
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

    pub(crate) fn on_semantic_search(&mut self) -> Task<Message> {
        let query = self.proj.semantic_query.trim().to_string();
        if query.is_empty() {
            return Task::none();
        }
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        self.drop_foreign_embed_index(&cfg);
        if self.proj.embed_index.entries.is_empty() {
            self.status = "Build the semantic index first (Semantic tab → Build index)".into();
            return Task::none();
        }
        self.proj.searching_semantic = true;
        // This submission supersedes any still in flight: its reply by the
        // sequence number, and its request by the guard that takes the old
        // one's place — dropped, it gives that request up.
        self.proj.semantic_seq += 1;
        let seq = self.proj.semantic_seq;
        let guard = RaiseOnDrop::default();
        let superseded = guard.flag();
        self.proj.inflight.semantic_search = Some(guard);
        let label = query.clone();
        let space = cfg.space();
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    embed::embed_batch_cancellable(&cfg, std::slice::from_ref(&query), &|| {
                        superseded.load(std::sync::atomic::Ordering::Relaxed)
                    })
                    .map(|mut v| v.pop().unwrap_or_default())
                })
                .await
                .unwrap_or_else(|_| Err("task join failed".into()))
            },
            move |result| {
                Message::Semantic(SemanticMsg::Results {
                    stamp: stamp.clone(),
                    seq,
                    query: label.clone(),
                    space: space.clone(),
                    result,
                })
            },
        )
    }

    pub(crate) fn on_semantic_results(
        &mut self,
        seq: u64,
        query: String,
        space: embed::Space,
        result: Result<Vec<f32>, String>,
    ) -> Task<Message> {
        // Supersession first: a newer submission owns the spinner, and an
        // older reply clearing it left the newer search looking finished.
        if seq != self.proj.semantic_seq {
            return Task::none();
        }
        self.proj.searching_semantic = false;
        self.proj.inflight.semantic_search = None;
        // Another window can change the stored configuration while the query
        // runs. Same-length vectors still cannot be compared across spaces.
        let stored = embed::stored_space();
        if stored != space || space.is_foreign(&self.proj.embed_index) {
            if stored.is_foreign(&self.proj.embed_index) {
                self.proj.embed_index = embed::Index::default();
            }
            self.proj.semantic_results.clear();
            self.status =
                "Embedding configuration changed — build the index and search again".into();
            return Task::none();
        }
        // A query whose vector length differs from the index's is an error
        // to show, not "0 matches" (cosine across two lengths is 0).
        match result.and_then(|qvec| {
            embed::search_checked(&self.proj.embed_index, &qvec, 20)
                .map(|hits| hits.into_iter().map(|(n, s)| (n.clone(), s)).collect())
        }) {
            Ok(hits) => {
                self.proj.semantic_results = hits;
                self.status = format!(
                    "{} semantic matches for “{query}”",
                    self.proj.semantic_results.len()
                );
            }
            Err(e) => self.status = format!("Search failed: {e}"),
        }
        Task::none()
    }

    /// Cosine similarity of a node's indexed embedding to the query vector, or 0
    /// when the node isn't in the index (e.g. a cursor anchor not yet embedded).
    pub(crate) fn node_score(&self, node: &explain::Node, qvec: &[f32]) -> f32 {
        self.proj
            .embed_index
            .entries
            .iter()
            .find(|e| &e.node == node)
            .map(|e| embed::cosine(qvec, &e.vec))
            .unwrap_or(0.0)
    }
}

impl App {
    /// Handle a [`SemanticMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_semantic(&mut self, message: SemanticMsg) -> Task<Message> {
        match message {
            SemanticMsg::BuildIndex => self.on_build_embeddings(),
            SemanticMsg::IndexBuilt { result, .. } => self.on_embeddings_built(result),
            SemanticMsg::IndexMerged { index, saved, .. } => {
                self.on_embeddings_merged(index.take(), saved)
            }
            SemanticMsg::QueryChanged(q) => {
                self.proj.semantic_query = q;
                Task::none()
            }
            SemanticMsg::Search => self.on_semantic_search(),
            SemanticMsg::Results {
                seq,
                query,
                space,
                result,
                ..
            } => self.on_semantic_results(seq, query, space, result),
            SemanticMsg::OpenNode(node) => match node {
                explain::Node::Function {
                    file,
                    name,
                    ordinal,
                } => {
                    // The node names the nth same-name function; jump to that
                    // one, not blindly the first.
                    let line = self.proj.symbol_index_by_file.get(&file).and_then(|syms| {
                        syms.iter()
                            .filter(|s| s.name == name)
                            .nth(ordinal as usize)
                            .or_else(|| syms.iter().find(|s| s.name == name))
                            .map(|s| s.line)
                    });
                    self.open_file(file, line, true)
                }
                explain::Node::File(p) => self.open_file(p, None, true),
                explain::Node::Folder(p) => self.show_explanation(explain::Node::Folder(p)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> embed::Entry {
        embed::Entry {
            node: explain::Node::Function {
                file: PathBuf::from("/p/a.rs"),
                name: name.into(),
                ordinal: 0,
            },
            hash: 0,
            vec: vec![1.0, 0.0],
        }
    }

    /// The index is loaded once, at project open, and then kept for the whole
    /// session — so a config change made by ANOTHER window or by hand-editing
    /// `config.toml` leaves this window ranking a query embedded at the new
    /// endpoint against vectors from the old space. Cosine answers confidently
    /// either way, so nothing about the results says they are meaningless.
    /// FIND must refuse and ask for a rebuild instead.
    #[test]
    fn find_refuses_an_index_the_live_embedding_config_disowns() {
        use crate::app::tests::{blank_app, data_dir_override, test_dir};
        let dir = test_dir("embed-space-find");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // `App::blank()` reads `trust.toml` / `connections.toml` through the
        // data dir, and the handler reads the config file from it, so isolate
        // both. The guard holds the env lock for the whole test and restores
        // the suite's isolated default on drop, even if an assertion unwinds.
        let _env = data_dir_override(&dir);
        std::fs::write(
            dir.join("config.toml"),
            "[embedding]\napi_key = \"sk\"\nmodel = \"m-b\"\n",
        )
        .unwrap();

        let mut app = blank_app();
        app.proj.semantic_query = "where is the parser".into();
        // Built under m-a, which is not what the config names now. The
        // endpoint matches, so the MODEL half is what has to disown it.
        app.proj.embed_index = embed::Index {
            model: "m-a".into(),
            base_url: "https://api.openai.com/v1".into(),
            entries: vec![entry("f")],
        };
        app.proj.semantic_results = vec![(entry("f").node, 0.9)];
        let _ = app.on_semantic_search();
        assert!(
            app.proj.embed_index.entries.is_empty(),
            "vectors from the old space stayed queryable"
        );
        assert!(
            app.proj.semantic_results.is_empty(),
            "results ranked in the old space stayed on screen"
        );
        assert!(
            !app.proj.searching_semantic,
            "the query was embedded to be ranked against a foreign index"
        );
        assert!(
            app.status.contains("Build the semantic index first"),
            "the refusal was not explained: {}",
            app.status
        );

        // An index that DOES belong to the live space is still queried — the
        // check must not cost a rebuild on every search.
        app.proj.embed_index = embed::Index {
            model: "m-b".into(),
            base_url: "https://api.openai.com/v1".into(),
            entries: vec![entry("f")],
        };
        let _ = app.on_semantic_search();
        assert_eq!(
            app.proj.embed_index.entries.len(),
            1,
            "an index in the live space was thrown away"
        );
        assert!(app.proj.searching_semantic, "the query never went out");

        drop(_env);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A search a newer one supersedes gives its request up: the endpoint
    /// sees the query's connection close as the reader searches again. Only
    /// the old search's answer used to be dropped (by its sequence number);
    /// its request ran on, holding a blocking thread and the connection,
    /// until the endpoint answered or the request's own limits ran out.
    #[test]
    fn a_superseded_search_lets_its_endpoint_go() {
        use crate::app::tests::{blank_app, data_dir_override, run_task, test_dir};
        use std::time::{Duration, Instant};
        let (base, arrived, closed) = clew_core::testutil::silent_http_endpoint();
        let dir = test_dir("embed-search-superseded");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _env = data_dir_override(&dir);
        std::fs::write(
            dir.join("config.toml"),
            format!("[embedding]\napi_key = \"sk\"\nmodel = \"m\"\nbase_url = \"{base}\"\n"),
        )
        .unwrap();

        let mut app = blank_app();
        app.proj.embed_index = embed::Index {
            model: "m".into(),
            base_url: base.clone(),
            entries: vec![entry("f")],
        };
        app.proj.semantic_query = "where is the parser".into();
        let first = app.on_semantic_search();
        assert!(app.proj.searching_semantic, "{}", app.status);
        let running = std::thread::spawn(move || run_task(first));
        arrived
            .recv_timeout(Duration::from_secs(10))
            .expect("the first query never reached the endpoint");

        let superseded_at = Instant::now();
        let _second = app.on_semantic_search();
        let (gone, at) = closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the superseded query kept its connection");
        assert!(gone);
        assert!(
            at.saturating_duration_since(superseded_at) < Duration::from_secs(2),
            "the connection closed {:?} after the newer search",
            at.saturating_duration_since(superseded_at)
        );
        // How it ended is the superseded search's own business.
        for msg in running.join().expect("the superseded search's task") {
            let _ = app.update(msg);
        }
        assert!(
            app.proj.searching_semantic,
            "the superseded search ended the newer one"
        );
    }
}
