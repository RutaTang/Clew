//! Navigation & project structure: graph layouts, import graph, finder, connect/remote, docs pages, scanning.

use crate::app::prelude::*;
use crate::*;

/// Whether a file's language is one clew fully supports in the graphs (the six
/// with import/call extraction). Files in any other language are kept out of the
/// Import and Call graphs entirely, so every node has a real language colour.
pub(crate) fn graph_language(path: &std::path::Path) -> bool {
    matches!(
        crate::highlight::detect(path),
        Some("rust" | "javascript" | "typescript" | "tsx" | "python" | "go" | "dart")
    )
}

impl App {
    /// Recompute the node-link layout for whichever overlay is open.
    pub(crate) fn refresh_graph_layout(&mut self) {
        self.graph_layout = match self.overlay {
            Some(Overlay::ProjectImports) => Some(self.import_graph_layout()),
            Some(Overlay::ProjectCalls) => Some(self.calls_graph_layout()),
            None => None,
        };
    }

    /// Force-directed layout of the import graph: nodes are files, sized by
    /// fan-in+fan-out, cycle members highlighted; edges are `use` dependencies.
    pub(crate) fn import_graph_layout(&self) -> graphlayout::Layout {
        let g = &self.import_graph;
        // Only graph the fully-supported languages; edges to any excluded file
        // fall away since `idx` is built from this filtered set.
        let files: Vec<PathBuf> = g
            .files()
            .into_iter()
            .filter(|f| graph_language(f))
            .collect();
        let idx: HashMap<PathBuf, usize> = files
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, f)| (f, i))
            .collect();
        let cyclic: HashSet<PathBuf> = self.import_cycles.iter().flatten().cloned().collect();
        let nodes = files
            .iter()
            .map(|f| graphlayout::NodeInput {
                label: file_label(f),
                file: f.clone(),
                weight: (g.fan_in(f) + g.fan_out(f) + 1) as f32,
                cyclic: cyclic.contains(f),
            })
            .collect();
        let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
        for f in &files {
            for e in g.imports(f) {
                if let imports::Target::Internal(t) = &e.target
                    && let (Some(&a), Some(&b)) = (idx.get(f), idx.get(t))
                {
                    edge_set.insert((a, b));
                }
            }
        }
        graphlayout::layout(nodes, edge_set.into_iter().collect())
    }

    /// Force-directed layout of the file-aggregated call graph: nodes are files
    /// sized by call degree; edges are cross-file call flow.
    pub(crate) fn calls_graph_layout(&self) -> graphlayout::Layout {
        let (all_files, all_edges) = self.project_calls.graph.file_graph();
        // Keep only the fully-supported languages, remapping edge indices onto
        // the filtered node set.
        let mut remap = vec![usize::MAX; all_files.len()];
        let mut files: Vec<PathBuf> = Vec::new();
        for (i, f) in all_files.iter().enumerate() {
            if graph_language(f) {
                remap[i] = files.len();
                files.push(f.clone());
            }
        }
        let edges: Vec<(usize, usize)> = all_edges
            .into_iter()
            .filter(|&(a, b)| remap[a] != usize::MAX && remap[b] != usize::MAX)
            .map(|(a, b)| (remap[a], remap[b]))
            .collect();
        let mut degree = vec![0usize; files.len()];
        for &(a, b) in &edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let nodes = files
            .iter()
            .enumerate()
            .map(|(i, f)| graphlayout::NodeInput {
                label: file_label(f),
                file: f.clone(),
                weight: (degree[i] + 1) as f32,
                cyclic: false,
            })
            .collect();
        graphlayout::layout(nodes, edges)
    }

    /// A resolver over the project's current file set (for building/refreshing
    /// the import graph). Cheap: in-memory path work plus one `go.mod` read.
    pub(crate) fn import_resolver(&self) -> Option<imports::Resolver> {
        let project = self.project.as_ref()?;
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        Some(imports::Resolver::new(&project.root, &files))
    }

    /// Rebuild the whole import graph from the per-file raw imports (after the
    /// index build) and refresh the tree.
    pub(crate) fn rebuild_import_graph(&mut self, raw: HashMap<PathBuf, Vec<imports::RawImport>>) {
        if let Some(resolver) = self.import_resolver() {
            self.import_graph = imports::ImportGraph::build(raw, &resolver, highlight::detect);
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
    }

    /// Re-resolve every edge against the current file set (after a file was
    /// created/deleted/renamed, which can change how other files resolve).
    pub(crate) fn reresolve_import_graph(&mut self) {
        if let Some(resolver) = self.import_resolver() {
            self.import_graph.reresolve(&resolver, highlight::detect);
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
    }

    /// The file the Imports tab is focused on — the active pane's file.
    pub(crate) fn import_focus(&self) -> Option<PathBuf> {
        self.active_viewer().map(|v| v.abs.clone())
    }

    /// Rebuild the import tree for the focus file, preserving the current
    /// direction and "expand all" state. Cheap — pure in-memory graph lookups.
    pub(crate) fn refresh_import_tree(&mut self) {
        let (Some(root), Some(focus)) = (
            self.project.as_ref().map(|p| p.root.clone()),
            self.import_focus(),
        ) else {
            self.import_tree = None;
            return;
        };
        let was_full = self.import_tree.as_ref().is_some_and(|t| t.full);
        let mut tree = imports::ImportTree::new(&self.import_graph, &root, focus, self.import_dir);
        if was_full {
            tree.expand_all(&self.import_graph, &root);
        }
        self.import_tree = Some(tree);
    }

    pub(crate) fn refresh_finder(&mut self) {
        match self.finder.mode {
            FinderMode::Files => {
                if let Some(p) = &self.project {
                    let files = p.files.clone();
                    self.finder.refresh_files(&files);
                }
            }
            FinderMode::Symbols => {
                let symbols = self.symbol_index.clone();
                self.finder.refresh_symbols(&symbols);
            }
        }
    }

    /// Gate every project open behind consent recorded **outside** the project.
    ///
    /// Consent used to be "a `.clew/` directory exists", but that directory is
    /// part of the repository — a hostile repo could ship one and grant itself
    /// permission, along with the `lsp.toml` inside it. The record now lives in
    /// clew's global data directory, keyed by the canonical root.
    pub(crate) fn request_open(&mut self, root: PathBuf) -> Task<Message> {
        if self.trust.is_root_trusted(&root) {
            return self.start_scan(root);
        }
        // Otherwise ask via an in-app modal (see ui::consent_modal).
        self.pending_consent = Some(root);
        Task::none()
    }

    pub(crate) fn start_scan(&mut self, root: PathBuf) -> Task<Message> {
        self.scanning = true;
        self.status = format!("Scanning {}…", root.display());
        // Preferred: let clew-server scan and return the tree (its `Tree` reply
        // builds the project via `handle_server_reply`).
        if self.server_tx.is_some() {
            if self.request_open_project(root.clone()) {
                return Task::none();
            }
            // Channel closed mid-session — fall through to a local scan.
        } else {
            // Server not up yet: defer. `ServerConnected` sends the OpenProject
            // once it is; `ServerUnavailable` falls back to a local scan. This
            // is what removes the duplicate scan at startup.
            self.pending_scan_root = Some(root);
            return Task::none();
        }
        self.local_scan(root)
    }

    /// Add (or update) a saved connection, de-duplicated by `user@host:port`, and
    /// persist the list. Most-recent first, so it heads the Connect modal's list.
    pub(crate) fn remember_connection(&mut self, conn: connect::SavedConnection) {
        self.saved_connections
            .retain(|c| !(c.user_host() == conn.user_host() && c.port == conn.port));
        self.saved_connections.insert(0, conn);
        if let Err(e) = connect::save(&self.saved_connections) {
            self.status = format!("Cannot save connections: {e}");
        }
    }

    /// Switch the server transport to `target`. Drops the current project (it
    /// lives on the old host) and the stale request channel; restarting the
    /// subscription brings up the new transport, which hands back a fresh channel
    /// via `ServerConnected`. The Connect modal, if open, moves to "connecting".
    pub(crate) fn connect_to(&mut self, target: connect::ConnTarget) {
        let label = target.label();
        self.project = None;
        self.panes = [None, None];
        self.split = false;
        self.active = 0;
        self.server_tx = None;
        self.pending_scan_root = None;
        self.scanning = false;
        self.connection = target;
        self.status = format!("Connecting to {label}…");
        if let Some(ui) = &mut self.connect {
            ui.stage = ConnectStage::Connecting { label };
        }
    }

    /// Show the remote folder picker for `path` (home when `None`) and request its
    /// listing. The reply (`DirListing`) fills it in via `handle_server_event`.
    pub(crate) fn enter_remote_browser(&mut self, path: Option<String>) {
        // Keep the current directory shown (dimmed) while the next one loads;
        // start empty when there was no browser yet.
        let (cwd, parent, entries) = match self.connect.as_mut().map(|u| &mut u.stage) {
            Some(ConnectStage::Browsing(b)) => (
                b.cwd.clone(),
                b.parent.clone(),
                std::mem::take(&mut b.entries),
            ),
            _ => (String::new(), None, Vec::new()),
        };
        if let Some(ui) = &mut self.connect {
            ui.stage = ConnectStage::Browsing(RemoteBrowser {
                cwd,
                parent,
                entries,
                loading: true,
            });
        }
        self.request_list_dir(path);
    }

    /// Send a `ListDir` for the remote folder picker (`None` = the login home).
    pub(crate) fn request_list_dir(&mut self, path: Option<String>) {
        let Some(tx) = self.server_tx.clone() else {
            return;
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(clew_protocol::ClientMessage {
            id,
            request: clew_protocol::Request::ListDir { path },
        });
    }

    /// Start a streaming answer for the Ask panel: push a pending turn, then feed
    /// it token-by-token — over the server (`ChatStream`, deltas routed by
    /// `handle_server_event`) when connected, else the provider locally. Returns
    /// the Task that pumps tokens into `AskDelta` / `AskStreamEnded`.
    pub(crate) fn start_ask_stream(
        &mut self,
        question: String,
        sources: Vec<(explain::Node, f32)>,
        cfg: llm::Config,
        system: String,
        messages: Vec<llm::ChatMsg>,
    ) -> Task<Message> {
        use iced::futures::SinkExt;
        self.ask_turns.push(AskTurn {
            question,
            answer_md: String::new(),
            answer: Vec::new(),
            sources,
            steps: Vec::new(),
            streaming: true,
        });
        self.asking = false;

        let stream_id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ChatStreamPiece>();

        // Server endpoint: register the channel and send the streaming request;
        // the deltas arrive as notifications. Otherwise stream locally.
        let local = if let Some(server_tx) = self.server_tx.clone() {
            self.chat_streams.lock().unwrap().insert(stream_id, tx);
            let msgs: Vec<clew_protocol::AiChatMsg> = messages
                .iter()
                .map(|m| clew_protocol::AiChatMsg {
                    role: m.role_str().to_string(),
                    content: m.content.clone(),
                })
                .collect();
            let _ = server_tx.send(clew_protocol::ClientMessage {
                id: stream_id,
                request: clew_protocol::Request::ChatStream {
                    stream: stream_id,
                    system,
                    messages: msgs,
                    max_tokens: 1024,
                },
            });
            None
        } else {
            Some((cfg, system, messages, tx))
        };

        let stream = iced::stream::channel(
            256,
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                let mut rx = rx;
                // Local endpoint: run the blocking provider call, feeding the channel.
                if let Some((cfg, system, messages, tx)) = local {
                    tokio::task::spawn_blocking(move || {
                        let result =
                            llm::complete_chat_stream(&cfg, &system, &messages, 1024, |d| {
                                let _ = tx.send(ChatStreamPiece::Delta(d.to_string()));
                            });
                        let _ = tx.send(ChatStreamPiece::Done(result.err()));
                    });
                }
                while let Some(piece) = rx.recv().await {
                    let (msg, done) = match piece {
                        ChatStreamPiece::Delta(t) => (Message::AskDelta(t), false),
                        ChatStreamPiece::Done(err) => (Message::AskStreamEnded(err), true),
                    };
                    if output.send(msg).await.is_err() || done {
                        break;
                    }
                }
            },
        );
        Task::run(stream, |m| m)
    }

    /// Start an agent turn for the Ask panel: the server explores the project
    /// with tools and streams steps / answer tokens back. Push a pending turn,
    /// register the piece channel, send `AgentAsk`, and pump the pieces into
    /// `AgentStepped` / `AskDelta` / `AgentTurnEnded`.
    pub(crate) fn start_agent_ask(&mut self, question: String) -> Task<Message> {
        use iced::futures::SinkExt;
        let Some(server_tx) = self.server_tx.clone() else {
            return Task::none();
        };
        // Client-side grounding the server can't see: the paused debugger state
        // and any pinned selections travel verbatim.
        let mut context = String::new();
        if let Some(state) = self.debug_context() {
            context.push_str(&state);
        }
        for pin in &self.ask_pins {
            context.push_str(&format!(
                "### Selected code — {} (L{})\n```\n{}\n```\n\n",
                pin.rel, pin.line, pin.code
            ));
        }
        // Replay recent turns so follow-ups resolve.
        const HIST_TURNS: usize = 6;
        let start = self.ask_turns.len().saturating_sub(HIST_TURNS);
        let mut history: Vec<clew_protocol::AiChatMsg> = Vec::new();
        for turn in &self.ask_turns[start..] {
            history.push(clew_protocol::AiChatMsg {
                role: "user".into(),
                content: turn.question.clone(),
            });
            history.push(clew_protocol::AiChatMsg {
                role: "assistant".into(),
                content: turn.answer_md.clone(),
            });
        }

        self.ask_turns.push(AskTurn {
            question: question.clone(),
            answer_md: String::new(),
            answer: Vec::new(),
            sources: Vec::new(),
            steps: Vec::new(),
            streaming: true,
        });
        self.asking = false;
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Ask;

        let stream_id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.agent_stream = Some(stream_id);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentPiece>();
        self.agent_streams.lock().unwrap().insert(stream_id, tx);
        let _ = server_tx.send(clew_protocol::ClientMessage {
            id: stream_id,
            request: clew_protocol::Request::AgentAsk {
                stream: stream_id,
                question,
                history,
                context,
            },
        });

        let stream = iced::stream::channel(
            256,
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                let mut rx = rx;
                while let Some(piece) = rx.recv().await {
                    let (msg, done) = match piece {
                        AgentPiece::Step(s) => (Message::AgentStepped(s), false),
                        AgentPiece::Delta(t) => (Message::AskDelta(t), false),
                        AgentPiece::Done(err) => (Message::AgentTurnEnded(err), true),
                    };
                    if output.send(msg).await.is_err() || done {
                        break;
                    }
                }
            },
        );
        Task::run(stream, |m| m)
    }

    /// Ask the server to (re)build the project's API docs. The `Docs` reply lands
    /// in `handle_server_event`.
    pub(crate) fn request_docs(&mut self) {
        let Some(tx) = self.server_tx.clone() else {
            return;
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tx
            .send(clew_protocol::ClientMessage {
                id,
                request: clew_protocol::Request::BuildDocs,
            })
            .is_ok()
        {
            self.docs.loading = true;
        }
    }

    /// Build the main-pane doc page for the item at (`rel`, `line`): the item
    /// itself plus its members (public unless "show all"), each with its doc
    /// comment parsed to markdown. Switches the main pane to the page.
    pub(crate) fn open_doc_page(&mut self, rel: &str, line: usize) {
        let Some(file) = self.docs.files.iter().find(|f| f.rel == rel) else {
            return;
        };
        let Some(item) = find_doc_item(&file.items, line) else {
            return;
        };
        let mut entries = Vec::new();
        flatten_doc(item, 0, self.docs.show_all, &mut entries);
        self.docs.page = Some(DocPage {
            rel: rel.to_string(),
            entries,
        });
        self.overview.showing = false;
        self.stats.showing = false;
    }

    /// Open the doc page for the symbol named `name` (from "View docs"). Switches
    /// to the DOCS tab. If the index isn't built yet, build it and resolve the
    /// name when it arrives.
    pub(crate) fn view_docs_for(&mut self, name: &str) {
        self.sidebar = SidebarTab::Docs;
        self.show_left_sidebar = true;
        if let Some((rel, line)) = find_doc_by_name(&self.docs.files, name) {
            self.open_doc_page(&rel, line);
        } else if self.docs.files.is_empty() {
            self.docs.pending_view = Some(name.to_string());
            self.request_docs();
        } else {
            self.status = format!("No docs for “{name}”");
        }
    }

    /// Ask the server to open `root` and return its tree. Records `root` as the
    /// pending scan so the `Tree` reply can build the project. Returns false if
    /// the request could not be sent (no server / channel closed).
    pub(crate) fn request_open_project(&mut self, root: PathBuf) -> bool {
        let Some(tx) = self.server_tx.clone() else {
            return false;
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = clew_protocol::Request::OpenProject {
            root: root.to_string_lossy().into_owned(),
        };
        if tx
            .send(clew_protocol::ClientMessage { id, request })
            .is_ok()
        {
            self.pending_scan_root = Some(root);
            true
        } else {
            false
        }
    }

    /// Scan the project on the client (fallback when the server is unavailable).
    pub(crate) fn local_scan(&mut self, root: PathBuf) -> Task<Message> {
        // `ScanDone` is accepted only for the root recorded here, so a slow
        // scan of a project the user has already left can't re-open it.
        self.pending_scan_root = Some(root.clone());
        Task::perform(
            async move {
                let fallback_root = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback_root,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::ScanDone,
        )
    }

    /// Re-scan the tree off-thread after a structural change, delivering the
    /// result as `TreeUpdated` (a light swap, not a full project reopen).
    pub(crate) fn rescan_tree(&self, root: PathBuf) -> Task<Message> {
        Task::perform(
            async move {
                let fallback = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::TreeUpdated,
        )
    }

    pub(crate) fn finder_open_index(&mut self, idx: usize) -> Task<Message> {
        self.finder.open = false;
        match self.finder.mode {
            FinderMode::Files => {
                let Some(entry) = self
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
                let Some(entry) = self.symbol_index.get(idx).cloned() else {
                    return Task::none();
                };
                self.open_file(entry.abs, Some(entry.line), true)
            }
        }
    }

    /// Open a file into the active pane, optionally jumping to a 1-based line.
    /// The function/method defined exactly at `(file, line1)`, if any — recorded
    /// with a history entry so it can be re-anchored across edits.
    pub(crate) fn symbol_name_at(&self, file: &Path, line1: usize) -> Option<String> {
        self.symbol_index_by_file.get(file)?.iter().find_map(|s| {
            (s.line == line1 && matches!(s.kind.as_str(), "function" | "method"))
                .then(|| s.name.clone())
        })
    }

    /// The live 1-based line of a noted symbol, resolved against the current
    /// index — `None` when the symbol no longer exists (an orphaned note).
    pub fn note_symbol_line(&self, rel: &str, symbol: &str) -> Option<usize> {
        let root = &self.project.as_ref()?.root;
        let abs = root.join(rel);
        self.symbol_index_by_file
            .get(&abs)?
            .iter()
            .find(|s| s.name == symbol)
            .map(|s| s.line)
    }

    /// Whether `(file, name)` is a test function, per the symbol index.
    pub fn is_test_symbol(&self, file: &Path, name: &str) -> bool {
        self.symbol_index_by_file
            .get(file)
            .is_some_and(|syms| syms.iter().any(|s| s.name == name && s.is_test))
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
        let source = self
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == caller_file)
            .map(|v| v.source.as_ref().clone())
            .or_else(|| std::fs::read_to_string(caller_file).ok())?;
        projectcalls::calls_of(&source, lang)
            .into_iter()
            .filter(|cs| cs.callee == callee && cs.caller.as_deref() == Some(caller))
            .map(|cs| cs.line)
            .min()
    }
}
