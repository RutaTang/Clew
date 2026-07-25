//! Debug sessions (DAP), reading-session persistence, file open/load, key handling, and code-view highlight computation.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// Begin a debug session from the project's `.clew/launch.json`. Spawns the
    /// adapter off-thread and streams its events back as `DapEvent` messages.
    pub(crate) fn start_debug(&mut self) -> Task<Message> {
        if self.debug.session.is_some() {
            self.status = "A debug session is already running".into();
            return Task::none();
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let cfg = match read_launch_config(&root) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.status = e;
                return Task::none();
            }
        };
        if !cfg.program.exists() {
            self.status = format!(
                "Program not found: {} — build it first",
                cfg.program.display()
            );
            return Task::none();
        }
        // Pick the language (explicit type, else the program's extension).
        let Some(lang) = dap::Lang::detect(cfg.type_hint.as_deref(), &cfg.program) else {
            self.status = format!("Unknown debug type {:?} in launch.json", cfg.type_hint);
            return Task::none();
        };
        let (program, args, cwd) = (cfg.program.clone(), cfg.args.clone(), cfg.cwd.clone());
        self.debug.session = Some(DebugSession {
            client: None,
            status: DebugStatus::Launching,
            thread_id: None,
            frames: Vec::new(),
            scopes: Vec::new(),
            watches: Vec::new(),
            output: Vec::new(),
            current: None,
            program: program.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            port: None,
        });
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug; // reveal the debug panel
        self.debug.last_fn = None;
        self.status = format!("Starting debugger — {}…", lang.label());

        // Preferred: spawn the debug adapter on clew-server (it must run where the
        // program does). Allocate its proc handle up front; the stream sets up the
        // proxy after it resolves the adapter binary.
        let proc = self.next_proc_id;
        self.next_proc_id += 1;
        let server_tx = self.server_tx.clone();

        let stream = iced::stream::channel(
            64,
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::SinkExt;
                // Resolve the adapter for this language (locates its binary + builds
                // the launch arguments). Off the UI thread as it may spawn xcrun/pip.
                let adapter = match dap::adapter::resolve(lang, &program, &args, &cwd) {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = output.send(Message::DebugFailed(e)).await;
                        return;
                    }
                };
                let port = match adapter.transport {
                    dap::client::Transport::Tcp(p) => Some(p),
                    dap::client::Transport::Stdio => None,
                };
                // Stdio adapters (lldb-dap) run on clew-server, proxied; TCP adapters
                // or a missing server fall back to a local spawn.
                let started = match (&adapter.transport, &server_tx) {
                    (dap::client::Transport::Stdio, Some(tx)) => {
                        let spawn = clew_protocol::Request::SpawnProcess {
                            proc,
                            cmd: adapter.command.to_string_lossy().into_owned(),
                            args: adapter.args.clone(),
                            cwd: Some(cwd.to_string_lossy().into_owned()),
                        };
                        let (stdin, stdout, feed) = proxy_transport(tx, proc, spawn);
                        // Register the output feed before the adapter can answer.
                        let _ = output.send(Message::RegisterProcFeed { proc, feed }).await;
                        dap::DapClient::connect(stdin, stdout).await
                    }
                    _ => {
                        dap::DapClient::start(
                            &adapter.command,
                            &adapter.args,
                            &cwd,
                            adapter.transport,
                        )
                        .await
                    }
                };
                let (client, mut events) = match started {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = output.send(Message::DebugFailed(e)).await;
                        return;
                    }
                };
                if let Err(e) = client.initialize().await {
                    let _ = output
                        .send(Message::DebugFailed(format!("initialize: {e}")))
                        .await;
                    return;
                }
                // Hand the client to the App *before* launching, so it holds the
                // handle when the `initialized` event arrives (it sends breakpoints).
                let _ = output
                    .send(Message::DapStarted {
                        client: client.clone(),
                        port,
                    })
                    .await;
                client.launch(adapter.launch);
                while let Some(ev) = events.recv().await {
                    if output.send(Message::DapEvent(ev)).await.is_err() {
                        break;
                    }
                }
                // Adapter closed: make sure the session tears down.
                let _ = output
                    .send(Message::DapEvent(dap::DapEvent::Terminated))
                    .await;
            },
        );
        Task::run(stream, |m| m)
    }

    /// Fold a DAP adapter event into the session state.
    pub(crate) fn on_dap_event(&mut self, ev: dap::DapEvent) -> Task<Message> {
        let Some(session) = self.debug.session.as_mut() else {
            return Task::none();
        };
        match ev {
            dap::DapEvent::Initialized => {
                // The adapter is ready for configuration: send every file's
                // breakpoints, then configurationDone to start execution.
                let Some(client) = session.client.clone() else {
                    return Task::none();
                };
                let bps: Vec<(PathBuf, BpList)> = self
                    .debug
                    .breakpoints
                    .iter()
                    .map(|(p, m)| {
                        (
                            p.clone(),
                            m.iter().map(|(l, bp)| (*l, bp.condition.clone())).collect(),
                        )
                    })
                    .collect();
                Task::perform(
                    async move {
                        for (file, lines) in bps {
                            let _ = client.set_breakpoints(&file, &lines).await;
                        }
                        let _ = client.configuration_done().await;
                    },
                    |()| Message::Noop,
                )
            }
            dap::DapEvent::Stopped(s) => {
                session.status = DebugStatus::Stopped;
                session.thread_id = s.thread_id;
                let Some(client) = session.client.clone() else {
                    return Task::none();
                };
                let tid = s.thread_id.unwrap_or(0);
                self.status = format!("Stopped: {}", s.reason);
                // Load the stack, then the top frame's scopes + variables.
                Task::perform(
                    async move {
                        let frames = client.stack_trace(tid).await.unwrap_or_default();
                        let mut scopes = Vec::new();
                        if let Some(top) = frames.first()
                            && let Ok(scs) = client.scopes(top.id).await
                        {
                            for sc in scs {
                                if sc.expensive || sc.variables_reference == 0 {
                                    continue; // skip Registers etc. by default
                                }
                                let vars = client
                                    .variables(sc.variables_reference)
                                    .await
                                    .unwrap_or_default();
                                scopes.push(DebugScope {
                                    name: sc.name,
                                    vars,
                                });
                            }
                        }
                        (frames, scopes)
                    },
                    |(frames, scopes)| Message::DapStopInspected { frames, scopes },
                )
            }
            dap::DapEvent::Continued { .. } => {
                session.status = DebugStatus::Running;
                session.current = None;
                session.frames.clear();
                session.scopes.clear();
                session.watches.clear();
                Task::none()
            }
            dap::DapEvent::Output(o) => {
                // Keep the tail bounded.
                if session.output.len() >= 500 {
                    session.output.remove(0);
                }
                session.output.push((o.category, o.text));
                Task::none()
            }
            dap::DapEvent::Exited { code } => {
                session.output.push((
                    "console".into(),
                    format!("Process exited with code {code}\n"),
                ));
                session.status = DebugStatus::Terminated;
                session.current = None;
                Task::none()
            }
            dap::DapEvent::Terminated => {
                session.status = DebugStatus::Terminated;
                session.current = None;
                session.frames.clear();
                session.scopes.clear();
                Task::none()
            }
            dap::DapEvent::StartDebugging(config) => {
                // js-debug: open a child session on the same adapter for the real
                // target, then drive its handshake (it owns the breakpoints/stack).
                let Some(port) = session.port else {
                    return Task::none();
                };
                let stream = iced::stream::channel(
                    64,
                    move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                        use iced::futures::SinkExt;
                        let (client, mut events) = match dap::DapClient::connect_tcp(port).await {
                            Ok(pair) => pair,
                            Err(e) => {
                                let _ = output.send(Message::DebugFailed(e)).await;
                                return;
                            }
                        };
                        if client.initialize().await.is_err() {
                            return;
                        }
                        let _ = output.send(Message::DapChildStarted(client.clone())).await;
                        client.launch(config);
                        while let Some(ev) = events.recv().await {
                            if output.send(Message::DapEvent(ev)).await.is_err() {
                                break;
                            }
                        }
                    },
                );
                Task::run(stream, |m| m)
            }
            dap::DapEvent::Other(_) => Task::none(),
        }
    }

    /// A snapshot of the paused debugger's runtime state (stopped location, call
    /// stack, and variable values), for grounding "Ask clew" answers in what's
    /// actually happening. `None` unless a session is stopped at a point.
    pub(crate) fn debug_context(&self) -> Option<String> {
        let session = self.debug.session.as_ref()?;
        if session.status != DebugStatus::Stopped {
            return None;
        }
        let mut s =
            String::from("### Runtime state (the program is PAUSED in the debugger right now)\n");
        if let Some((path, line)) = &session.current {
            s.push_str(&format!("Paused at {}:{}\n", self.rel_of(path), line));
        }
        if !session.frames.is_empty() {
            s.push_str("Call stack (innermost first):\n");
            for f in session.frames.iter().take(8) {
                let loc = f
                    .path
                    .as_ref()
                    .map(|p| format!(" ({}:{})", self.rel_of(p), f.line))
                    .unwrap_or_default();
                s.push_str(&format!("- {}{}\n", f.name, loc));
            }
        }
        for sc in &session.scopes {
            if sc.vars.is_empty() {
                continue;
            }
            s.push_str(&format!("Variables — {} (current frame):\n", sc.name));
            for v in sc.vars.iter().take(40) {
                s.push_str(&format!("- {} = {}\n", v.name, v.value));
            }
        }
        s.push('\n');
        Some(s)
    }

    /// Push one file's breakpoints (line + condition) to a live adapter. No-op
    /// when no session is running.
    pub(crate) fn push_breakpoints(&self, path: &Path) -> Task<Message> {
        let Some(client) = self.debug.session.as_ref().and_then(|s| s.client.clone()) else {
            return Task::none();
        };
        let lines: BpList = self
            .debug
            .breakpoints
            .get(path)
            .map(|m| m.iter().map(|(l, bp)| (*l, bp.condition.clone())).collect())
            .unwrap_or_default();
        let p = path.to_path_buf();
        Task::perform(
            async move {
                let _ = client.set_breakpoints(&p, &lines).await;
            },
            |()| Message::Noop,
        )
    }

    /// Re-evaluate all watch expressions in the current frame (on each stop, or
    /// when a watch is added). No-op unless paused with watches set.
    pub(crate) fn eval_watches(&self) -> Task<Message> {
        let Some(session) = self.debug.session.as_ref() else {
            return Task::none();
        };
        if session.status != DebugStatus::Stopped || self.debug.watches.is_empty() {
            return Task::none();
        }
        let (Some(client), Some(frame)) = (session.client.clone(), session.frames.first()) else {
            return Task::none();
        };
        let frame_id = frame.id;
        let exprs = self.debug.watches.clone();
        Task::perform(
            async move {
                let mut out = Vec::with_capacity(exprs.len());
                for e in exprs {
                    let v = client
                        .evaluate(&e, frame_id)
                        .await
                        .unwrap_or_else(|err| format!("⚠ {err}"));
                    out.push((e, v));
                }
                out
            },
            Message::DebugWatchesEvaluated,
        )
    }

    /// Send a stepping / continue command to the adapter.
    pub(crate) fn debug_control(&mut self, cmd: DebugCmd) -> Task<Message> {
        let Some(session) = self.debug.session.as_mut() else {
            return Task::none();
        };
        let (Some(client), Some(tid)) = (session.client.clone(), session.thread_id) else {
            return Task::none();
        };
        session.status = DebugStatus::Running;
        session.current = None;
        Task::perform(
            async move {
                let _ = match cmd {
                    DebugCmd::Continue => client.continue_(tid).await,
                    DebugCmd::StepOver => client.next(tid).await,
                    DebugCmd::StepIn => client.step_in(tid).await,
                    DebugCmd::StepOut => client.step_out(tid).await,
                };
            },
            |()| Message::Noop,
        )
    }

    /// Persist the navigation tree to the project's `.clew/`, ignoring errors
    /// (a read-only project just keeps its history for the session).
    pub(crate) fn save_history(&self) {
        if let Some(root) = self.project.as_ref().map(|p| &p.root) {
            let _ = history::save(root, &self.history);
        }
    }

    pub(crate) fn save_notes(&mut self) {
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = notes::save(&root, &self.notes)
        {
            self.status = format!("Cannot write .clew/notes.json: {e}");
        }
    }

    pub(crate) fn open_file(
        &mut self,
        abs: PathBuf,
        line: Option<usize>,
        push: bool,
    ) -> Task<Message> {
        // Opening a file leaves the overview / stats / docs page for the code, and
        // ends any time-travel session (which would otherwise stay active-but-hidden
        // and keep capturing Esc/←/→ for a file that's no longer shown).
        self.overview.showing = false;
        self.stats.showing = false;
        self.docs.page = None;
        self.time_travel = None;
        if push {
            // Remember the symbol at the target so the trail can re-anchor to it
            // after edits shift its line (see `reanchor` in FilesRehashed).
            let label = line.and_then(|l| self.symbol_name_at(&abs, l));
            self.history.push(
                Loc {
                    path: abs.clone(),
                    line,
                },
                label,
            );
            self.save_history();
        }
        // A jump lands the reader in the code view.
        self.code_focused = true;
        let pane = self.active;
        let line_height = self.line_height();
        // Same file already in the active pane: move the cursor and scroll.
        if let Some(v) = self.active_viewer_mut()
            && v.abs == abs
        {
            v.target_line = line;
            if let Some(l) = line {
                let l0 = l.saturating_sub(1);
                v.reveal(l0); // expand any fold hiding the jump target
                v.caret = Some((l0, 0));
            }
            let y = v.scroll_offset_for(line, line_height);
            v.scroll_y = y;
            let scroll =
                operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
            return self.follow_caret(scroll);
        }
        let rel = self.rel_of(&abs);
        // A go-to-def target outside the project (a dependency or stdlib source
        // the LSP resolved) would be refused by the server, whose ReadFile
        // enforces the project boundary. For a LOCAL server, read it directly,
        // read-only — the client already resolved it via the LSP, so reading a
        // dep's source is safe and is core to a code reader. Keep routing
        // through the server for a REMOTE connection, where the boundary is a
        // real security guard and the file lives on the remote host anyway.
        let external_local = self
            .project
            .as_ref()
            .is_some_and(|p| !abs.starts_with(&p.root))
            && !self.connection.is_remote();
        // Preferred: fetch the file from clew-server — it reads, highlights, and
        // extracts symbols/docs/inactive server-side. The reply arrives as
        // Event::FileContent and lands via `apply_file_content`.
        if !external_local && let Some(tx) = self.server_tx.clone() {
            let id = self
                .next_req_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let request = clew_protocol::Request::ReadFile {
                rel: rel.clone(),
                target: self.target_spec(),
            };
            if tx
                .send(clew_protocol::ClientMessage { id, request })
                .is_ok()
            {
                self.pending_reads
                    .insert(id, ReadKind::Open { pane, target: line });
                self.status = format!("Loading {rel}…");
                return Task::none();
            }
        }
        // Fallback: server not up — read + highlight locally.
        self.status = format!("Loading {rel}…");
        Task::perform(load_file(pane, abs, line), |(pane, abs, target, result)| {
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            }
        })
    }

    pub(crate) fn on_file_loaded(
        &mut self,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    ) -> Task<Message> {
        let rel = self.rel_of(&abs);
        let content = match result {
            Err(e) => {
                self.status = format!("{rel}: {e}");
                return Task::none();
            }
            Ok(content) => content,
        };

        let lang_key = highlight::detect(&abs);
        let source = Arc::new(content);
        let lines = highlight::plain_lines(&source);
        let line_height = self.line_height();
        let old_viewport = self.panes[pane].as_ref().map(|v| v.viewport_h);
        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        // Put the block cursor on the jump target (or the top of the file).
        v.caret = Some((target.map(|t| t.saturating_sub(1)).unwrap_or(0), 0));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        // Just the path here; the right status segment already reports line count.
        self.status = v.rel.clone();
        self.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        self.registry
            .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        // Point the Imports tab at the newly focused file.
        if pane == self.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        // Start (or reuse) a language server for this file and open the doc.
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        let content = self.content_tasks(abs, source, lang_key);
        // Symbols arrive later via `Highlighted`; follow_caret there resolves the
        // enclosing function. Here it shows the file until then.
        self.follow_caret(Task::batch([scroll, lsp_task, content]))
    }

    /// Off-thread re-highlight + git-info tasks for a file's current source,
    /// shared by initial load and live refresh. Both deliver `Highlighted` /
    /// `GitInfoLoaded` keyed by `abs`, so they route to whatever pane shows it.
    pub(crate) fn content_tasks(
        &self,
        abs: PathBuf,
        source: Arc<String>,
        lang_key: Option<&'static str>,
    ) -> Task<Message> {
        let hl_abs = abs.clone();
        let hl_source = source.clone();
        let target = self.reading_target.clone();
        let highlight_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lines = highlight::highlight_lines(&hl_source, lang_key);
                    let symbols = lang_key
                        .map(|key| outline::extract(&hl_source, key))
                        .unwrap_or_default();
                    // Author's doc comments, reusing the symbols just parsed.
                    let docs = lang_key
                        .map(|key| docs::extract(&hl_source, key, &symbols))
                        .unwrap_or_default();
                    // Inactive `#[cfg]` lines for the reading target (dimmed).
                    let inactive = lang_key
                        .map(|key| inactive::inactive_lines(&hl_source, key, &target))
                        .unwrap_or_default();
                    (lines, symbols, docs, inactive)
                })
                .await
                .unwrap_or_default()
            },
            move |(lines, symbols, docs, inactive)| Message::Highlighted {
                abs: hl_abs.clone(),
                lines,
                symbols,
                docs,
                inactive,
            },
        );

        let git_task = match self.project.as_ref().map(|p| p.root.clone()) {
            Some(root) => {
                let file = abs.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || git::info(&root, &file).map(Arc::new))
                            .await
                            .ok()
                            .flatten()
                    },
                    move |info| Message::GitInfoLoaded {
                        abs: abs.clone(),
                        info,
                    },
                )
            }
            None => Task::none(),
        };
        Task::batch([highlight_task, git_task])
    }

    /// Keep the top visible line stable across a line-height change.
    pub(crate) fn rescale_scroll(&mut self, old_line_height: f32) -> Task<Message> {
        let new_line_height = self.line_height();
        if (new_line_height - old_line_height).abs() < f32::EPSILON {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for (pane, slot) in self.panes.iter_mut().enumerate() {
            if let Some(v) = slot {
                let first = v.scroll_y / old_line_height;
                v.scroll_y = first * new_line_height;
                tasks.push(operation::scroll_to(
                    ui::code_scroll_id(pane),
                    AbsoluteOffset {
                        x: 0.0,
                        y: v.scroll_y,
                    },
                ));
            }
        }
        Task::batch(tasks)
    }

    pub(crate) fn handle_key(
        &mut self,
        key: keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        let cmd = modifiers.command();

        // Rebinding capture takes priority: the next chord becomes the binding.
        if let Some(action) = self.rebinding {
            return self.capture_rebind(action, &key, modifiers);
        }
        // The tutorial is modal: → / Enter advance, ← goes back, Esc leaves, and
        // every other key is swallowed so nothing acts behind the overlay.
        if self.tutorial.is_some() {
            return match key.as_ref() {
                Key::Named(Named::ArrowRight | Named::Enter | Named::Space) => {
                    self.on_tutorial_step(1)
                }
                Key::Named(Named::ArrowLeft) => self.on_tutorial_step(-1),
                Key::Named(Named::Escape) => self.on_tutorial_exit(),
                _ => Task::none(),
            };
        }
        // While the shortcuts panel is open (and not capturing), only Esc
        // closes it; swallow other keys so nothing acts behind the modal.
        if self.show_shortcuts {
            if matches!(key.as_ref(), Key::Named(Named::Escape)) {
                self.show_shortcuts = false;
                self.keymap_notice = None;
            }
            return Task::none();
        }
        // Time travel history navigation uses COMMAND chords — Cmd+←/Cmd+h step
        // to an older commit, Cmd+→/Cmd+l to a newer one — so plain ←/→/h/l stay
        // free for reading. Esc exits. Handled before the keymap so these chords
        // drive time travel (overriding e.g. Cmd+← = back) while a session is on.
        if let Some(tt) = self.time_travel.as_ref() {
            let (idx, n) = (tt.idx, tt.commits.len());
            match key.as_ref() {
                Key::Named(Named::Escape) => return self.update(Message::TimeTravelExit),
                Key::Named(Named::ArrowLeft) | Key::Character("h") if cmd && idx + 1 < n => {
                    return self.update(Message::TimeTravelGoto(idx + 1));
                }
                Key::Named(Named::ArrowRight) | Key::Character("l") if cmd && idx > 0 => {
                    return self.update(Message::TimeTravelGoto(idx - 1));
                }
                _ => {}
            }
        }
        // Command chords (those carrying ⌘/⌥/⌃) are dispatched through the
        // customizable keymap. Only modifier-carrying chords are eligible, so
        // the single-key reading motions and text input below stay untouched.
        if (cmd || modifiers.alt() || modifiers.control())
            && let Some(chord) = keymap::Chord::from_event(&key, modifiers)
            && let Some(action) = self.keymap.action_for(&chord)
            && let Some(task) = self.run_command_action(action)
        {
            return task;
        }

        // While time-travelling, swallow any remaining (non-command) keys so
        // plain reading motions don't act on the live file hidden behind the view.
        if self.time_travel.is_some() {
            return Task::none();
        }

        match key.as_ref() {
            // In-file find bar: Enter next, Shift+Enter prev.
            Key::Named(Named::Enter) if self.find.open => {
                self.update(Message::FindStep(if modifiers.shift() { -1 } else { 1 }))
            }
            Key::Named(Named::Escape) => {
                self.pending_g = false;
                self.pending_z = false;
                if self.context_menu.is_some() {
                    self.context_menu = None;
                    return Task::none();
                }
                if self.find.open {
                    return self.update(Message::FindClosed);
                }
                if self.finder.open {
                    return self.update(Message::FinderClosed);
                }
                if let Some(v) = self.active_viewer_mut() {
                    v.selection = None;
                    v.target_line = None;
                }
                Task::none()
            }
            Key::Named(Named::ArrowDown) if self.finder.open => {
                self.finder.move_selection(1);
                Task::none()
            }
            Key::Named(Named::ArrowUp) if self.finder.open => {
                self.finder.move_selection(-1);
                Task::none()
            }
            // -------- Vim-style read-only cursor (only when the code view has
            // focus, so it never steals keys from a text input) --------
            _ if cmd
                || self.finder.open
                || self.context_menu.is_some()
                || !self.code_focused
                || self.active_viewer().is_none() =>
            {
                Task::none()
            }
            // Two-key `g` prefix: gg / gd / gr / gi / gy / gc.
            _ if self.pending_g => {
                self.pending_g = false;
                match key.as_ref() {
                    Key::Character("g") => self.move_cursor(viewer::Motion::FileStart),
                    Key::Character("d") => self.goto_at_cursor(GotoKind::Definition),
                    Key::Character("r") => self.goto_at_cursor(GotoKind::References),
                    Key::Character("i") => self.goto_at_cursor(GotoKind::Implementation),
                    Key::Character("y") => self.goto_at_cursor(GotoKind::TypeDefinition),
                    Key::Character("c") => self.update(Message::CallHierarchyRequested),
                    _ => Task::none(),
                }
            }
            Key::Character("g") => {
                self.pending_g = true;
                Task::none()
            }
            // Two-key `z` prefix for folding: za toggle, zR open all, zM close all.
            _ if self.pending_z => {
                self.pending_z = false;
                match key.as_ref() {
                    Key::Character("a") => self.fold_toggle_at_cursor(),
                    Key::Character("R") => self.fold_all(false),
                    Key::Character("M") => self.fold_all(true),
                    _ => {}
                }
                Task::none()
            }
            Key::Character("z") => {
                self.pending_z = true;
                Task::none()
            }
            Key::Character("h") | Key::Named(Named::ArrowLeft) => {
                self.move_cursor(viewer::Motion::Left)
            }
            Key::Character("l") | Key::Named(Named::ArrowRight) => {
                self.move_cursor(viewer::Motion::Right)
            }
            Key::Character("k") | Key::Named(Named::ArrowUp) => {
                self.move_cursor(viewer::Motion::Up)
            }
            Key::Character("j") | Key::Named(Named::ArrowDown) => {
                self.move_cursor(viewer::Motion::Down)
            }
            Key::Character("w") => self.move_cursor(viewer::Motion::WordForward),
            Key::Character("b") => self.move_cursor(viewer::Motion::WordBack),
            Key::Character("0") => self.move_cursor(viewer::Motion::LineStart),
            Key::Character("$") => self.move_cursor(viewer::Motion::LineEnd),
            Key::Character("G") => self.move_cursor(viewer::Motion::FileEnd),
            _ => Task::none(),
        }
    }

    /// Hover peek assembled locally, no LSP: the same-file symbol's doc comment
    /// plus, for Rust, the project-wide structure of the type or trait under the
    /// cursor ("impl …" / "Implementors …"). `None` when neither applies, so the
    /// caller falls through to the language server.
    pub(crate) fn local_peek(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
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
            && let Some(summary) = self.structure.summary_line(&word)
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
    pub(crate) fn hover_summary(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
        let word = analyze::word_at(&v.lines, line, col)?;
        let usable = |s: &str| (!explain::is_error_summary(s)).then(|| ui::first_sentence(s));
        // Same-file definition wins (unambiguous).
        if let Some(c) = self.explain.cache.get(&explain::Node::Function {
            file: v.abs.clone(),
            name: word.clone(),
        }) {
            return usable(&c.summary);
        }
        // Otherwise, only if exactly one explained function has this name.
        let mut hit: Option<&str> = None;
        for (node, c) in &self.explain.cache {
            if let explain::Node::Function { name, .. } = node
                && name == &word
            {
                if hit.is_some() {
                    return None; // ambiguous
                }
                hit = Some(&c.summary);
            }
        }
        hit.and_then(usable)
    }

    /// The LSP diagnostic covering (`line`, `col`) in `pane`, as a labelled
    /// message ("Error: …" / "Warning: …"), so hovering a red-underlined symbol
    /// says what is wrong. Prefers the most severe diagnostic at that spot. Uses
    /// the same char→display-column mapping as the underline rendering.
    pub(crate) fn diagnostic_at(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
        let lang = v.lang_key?;
        let LspSlot::Ready(client) = self.lsp.get(lang)? else {
            return None;
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        client
            .diagnostics(&v.abs)
            .into_iter()
            .filter(|d| d.line == line)
            .filter(|d| {
                let raw = v.source_line(d.line).unwrap_or("");
                let c0 = viewer::display_col_from_char(raw, d.char_start, utf16);
                let c1 = viewer::display_col_from_char(raw, d.char_end, utf16).max(c0 + 1);
                (c0..c1).contains(&col)
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

    /// Run a rebindable command action. Returns `None` when the action declines
    /// in the current context (so the key falls through — e.g. ⌘C inside the
    /// finder input should copy text, not the code selection).
    pub(crate) fn run_command_action(&mut self, action: keymap::Action) -> Option<Task<Message>> {
        use keymap::Action::*;
        Some(match action {
            OpenFile => self.update(Message::FinderOpened(FinderMode::Files)),
            OpenSymbol => self.update(Message::FinderOpened(FinderMode::Symbols)),
            ProjectSearch => self.update(Message::SidebarTabPicked(SidebarTab::Search)),
            FindInFile => self.update(Message::FindOpened),
            CopySelection => {
                if self.finder.open {
                    return None;
                }
                self.update(Message::CopySelection)
            }
            ToggleBookmark => self.update(Message::BookmarkToggled),
            GotoLine => self.update(Message::GotoLineRequested),
            ToggleSplit => self.update(Message::ToggleSplit),
            ZoomIn => self.update(Message::FontSizeDelta(1.0)),
            ZoomOut => self.update(Message::FontSizeDelta(-1.0)),
            ZoomReset => self.update(Message::FontSizeReset),
            GoBack => self.update(Message::GoBack),
            GoForward => self.update(Message::GoForward),
        })
    }

    /// Capture a keypress as the new binding for `action`. Esc cancels; keys
    /// without a ⌘/⌥/⌃ modifier or that collide with another action are
    /// rejected with an inline notice (capture stays active so the user can
    /// try again). A successful bind is persisted immediately.
    pub(crate) fn capture_rebind(
        &mut self,
        action: keymap::Action,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        use keyboard::key::Named;
        if matches!(key.as_ref(), keyboard::Key::Named(Named::Escape)) {
            self.rebinding = None;
            self.keymap_notice = None;
            return Task::none();
        }
        let Some(chord) = keymap::Chord::from_event(key, modifiers) else {
            self.keymap_notice = Some("Unsupported key".into());
            return Task::none();
        };
        if !chord.is_command() {
            self.keymap_notice = Some("Shortcut must include ⌘, ⌥, or ⌃".into());
            return Task::none();
        }
        if let Some(other) = self.keymap.conflict(&chord, action) {
            self.keymap_notice = Some(format!("Already used by “{}”", other.label()));
            return Task::none();
        }
        self.keymap.rebind(action, chord);
        self.rebinding = None;
        self.keymap_notice = None;
        if let Err(e) = self.keymap.save() {
            self.status = format!("Could not save shortcuts: {e}");
        }
        Task::none()
    }

    /// Move the active pane's block cursor and scroll it into view.
    pub(crate) fn move_cursor(&mut self, motion: viewer::Motion) -> Task<Message> {
        let pane = self.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.move_caret(motion);
        let (line, _) = v.caret.unwrap_or((0, 0));
        // Keep the cursor line within the viewport (in display rows, so folds
        // above it are accounted for).
        let top = v.row_of(line) as f32 * line_height;
        let bottom = top + line_height;
        if top < v.scroll_y {
            v.scroll_y = top;
        } else if bottom > v.scroll_y + v.viewport_h {
            v.scroll_y = bottom - v.viewport_h;
        }
        let y = v.scroll_y;
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let follow = self.follow_caret(scroll);
        Task::batch([follow, self.sync_reading_context()])
    }

    /// Toggle the fold enclosing the caret (`za`).
    pub(crate) fn fold_toggle_at_cursor(&mut self) {
        if let Some(v) = self.active_viewer_mut() {
            let line = v.caret.map(|(l, _)| l).unwrap_or(0);
            if let Some(header) = v.fold_header_for(line) {
                v.toggle_fold(header);
            }
        }
    }

    /// Collapse (`zM`) or expand (`zR`) every fold in the active pane.
    pub(crate) fn fold_all(&mut self, collapse: bool) {
        if let Some(v) = self.active_viewer_mut() {
            if collapse {
                v.collapse_all();
            } else {
                v.expand_all();
            }
        }
    }

    /// Toggle "skim" for the active file: fold every function/method body down
    /// to its signature (which still shows its inline summary), so the file
    /// reads as an annotated table of contents. Uses the symbol index to fold
    /// only bodies, leaving impl/mod blocks open so every signature stays shown.
    pub(crate) fn skim_active_file(&mut self) {
        let Some(v) = self.active_viewer() else {
            return;
        };
        let sig_lines: Vec<usize> = self
            .symbol_index_by_file
            .get(&v.abs)
            .map(|syms| {
                syms.iter()
                    .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                    .map(|s| s.line.saturating_sub(1)) // 1-based symbol line → 0-based
                    .collect()
            })
            .unwrap_or_default();
        if let Some(v) = self.active_viewer_mut() {
            v.skim_bodies(&sig_lines);
        }
    }

    /// Move the cursor to the current find match and scroll it into view.
    pub(crate) fn jump_to_find_match(&mut self) -> Task<Message> {
        let Some((line, col, _)) = self.find.current_match() else {
            return Task::none();
        };
        let pane = self.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.caret = Some((line, col));
        // Center-ish the match line (display rows account for folds).
        let top = v.row_of(line) as f32 * line_height;
        if top < v.scroll_y || top + line_height > v.scroll_y + v.viewport_h {
            v.scroll_y = (top - v.viewport_h / 3.0).max(0.0);
        }
        let y = v.scroll_y;
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        self.follow_caret(scroll)
    }

    /// Extra span highlights for the code view of `pane`: find matches, or
    /// (when not finding) the occurrences of the identifier under the cursor
    /// and the matching bracket.
    pub fn code_highlights(&self, pane: usize, v: &Viewer) -> Vec<codeview::Hl> {
        use codeview::{Hl, HlKind};
        let mut out = Vec::new();
        if pane != self.active {
            return out;
        }

        // Diagnostic underlines (always shown, from the LSP server).
        if let Some(lang) = v.lang_key
            && let Some(LspSlot::Ready(client)) = self.lsp.get(lang)
        {
            let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
            for d in client.diagnostics(&v.abs) {
                let raw = v.source_line(d.line).unwrap_or("");
                let c0 = viewer::display_col_from_char(raw, d.char_start, utf16);
                let c1 = viewer::display_col_from_char(raw, d.char_end, utf16).max(c0 + 1);
                out.push(Hl {
                    line: d.line,
                    col0: c0,
                    col1: c1,
                    kind: match d.severity {
                        1 => HlKind::DiagError,
                        2 => HlKind::DiagWarn,
                        _ => HlKind::DiagHint,
                    },
                });
            }
        }

        if self.find.open {
            for (i, &(line, col0, col1)) in self.find.matches.iter().enumerate() {
                out.push(Hl {
                    line,
                    col0,
                    col1,
                    kind: if i == self.find.current {
                        HlKind::FindCurrent
                    } else {
                        HlKind::FindMatch
                    },
                });
            }
            return out;
        }

        // Cursor-derived aids, only while reading (code has focus).
        if !self.code_focused {
            return out;
        }
        let Some((line, col)) = v.caret else {
            return out;
        };

        // Occurrences of the identifier under the cursor (2+ to be useful).
        if let Some(word) = analyze::word_at(&v.lines, line, col) {
            let occ = analyze::occurrences(&word, &v.lines, 500);
            if occ.len() > 1 {
                for (l, c0, c1) in occ {
                    out.push(Hl {
                        line: l,
                        col0: c0,
                        col1: c1,
                        kind: HlKind::Occurrence,
                    });
                }
            }
        }

        // Matching bracket pair.
        if let Some((ml, mc)) = analyze::matching_bracket(&v.lines, line, col) {
            out.push(Hl {
                line,
                col0: col,
                col1: col + 1,
                kind: HlKind::Bracket,
            });
            out.push(Hl {
                line: ml,
                col0: mc,
                col1: mc + 1,
                kind: HlKind::Bracket,
            });
        }
        out
    }

    /// Inline blame annotation for the caret line: `author, when · summary`.
    pub fn blame_annotation(&self, v: &Viewer) -> Option<(usize, String)> {
        let git = v.git.as_ref()?;
        let (line, _) = v.caret?;
        let b = git.blame_for(line)?;
        if b.commit.is_empty() {
            return None;
        }
        let text = if b.uncommitted {
            "· Uncommitted change".to_string()
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(b.time);
            let mut summary = b.summary.clone();
            if summary.chars().count() > 60 {
                summary = summary.chars().take(59).collect::<String>() + "…";
            }
            format!(
                "{}, {} · {}",
                b.author,
                git::relative_time(b.time, now),
                summary
            )
        };
        Some((line, text))
    }

    /// Sticky-scroll header lines for a viewer at its current scroll position.
    pub fn sticky_headers(&self, v: &Viewer) -> Vec<usize> {
        let row = (v.scroll_y / self.line_height()) as usize;
        let first_visible = v.line_at_row(row);
        // Read enclosing headers off the precomputed fold ranges — cheap enough
        // to recompute each frame, so sticky scroll stays smooth in huge files.
        analyze::sticky_headers(&v.folds, first_visible, 5)
    }

    pub(crate) fn rel_of(&self, abs: &Path) -> String {
        self.project
            .as_ref()
            .and_then(|p| abs.strip_prefix(&p.root).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| abs.display().to_string())
    }
}
