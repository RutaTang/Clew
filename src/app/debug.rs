//! The debugger (DAP): starting a session from the launch config (adapter
//! install consent included), adapter events, stops and their inspection,
//! breakpoints and their verdicts, watches, stepping, and ending a session.
//!
//! Its messages, [`DebugMsg`], arrive through `App::update_debug`.

use crate::app::prelude::*;
use crate::*;

/// Chunks of program output the debug panel retains.
const DEBUG_OUTPUT_MAX_CHUNKS: usize = 500;

/// Bytes of program output the debug panel retains. Both caps are needed, and
/// neither implies the other: the chunk cap bounds how many entries the panel
/// lays out but says nothing about their size, so a debuggee making large
/// writes (each forwarded as one output event) pins the byte cost of 500
/// arbitrary chunks; a byte cap alone would let a flood of one-byte lines cost
/// a Vec entry each and bog the layout down at a trivial memory cost.
const DEBUG_OUTPUT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Append one chunk to a session's retained output, trimming oldest-first
/// until both caps hold. The total is recomputed per push rather than kept in
/// the session: this runs once per output event over a few hundred short
/// entries, far cheaper than the layout it feeds.
fn push_debug_output(session: &mut DebugSession, category: String, text: String) {
    session.output.push((category, text));
    let mut bytes: usize = session.output.iter().map(|(c, t)| c.len() + t.len()).sum();
    // The newest chunk is never trimmed away to satisfy the byte cap — it is
    // already bounded where it enters the client, and showing a truncated tail
    // beats showing nothing at all.
    while session.output.len() > DEBUG_OUTPUT_MAX_CHUNKS
        || (bytes > DEBUG_OUTPUT_MAX_BYTES && session.output.len() > 1)
    {
        let (c, t) = session.output.remove(0);
        bytes -= c.len() + t.len();
    }
}

impl App {
    /// Begin a debug session from the project's `.clew/launch.json`. Spawns the
    /// adapter off-thread and streams its events back as `DapEvent` messages.
    pub(crate) fn start_debug(&mut self) -> Task<Message> {
        if self.debug.session.is_some() {
            self.status = "A debug session is already running".into();
            return Task::none();
        }
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // A remote project's launch.json lives in the REMOTE .clew (fetched
        // over the protocol inside the stream, along with the adapter — the
        // program, config, and adapter binary all live on that host, so
        // nothing here may read or probe the local filesystem).
        let remote = self.connection.is_remote();
        let (program, args, cwd, lang) = if remote {
            (root.clone(), Vec::new(), root.clone(), None)
        } else {
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
            (cfg.program, cfg.args, cfg.cwd, Some(lang))
        };
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
            addr: None,
        });
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug; // reveal the debug panel
        self.debug.last_fn = None;
        // A new run: its trace starts empty.
        self.debug.trace.clear();
        self.debug.trace_cut = false;
        self.debug.trace_rev += 1;
        self.debug.trace_program = program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.debug.pending_reason.clear();
        self.status = match lang {
            Some(lang) => format!("Starting debugger — {}…", lang.label()),
            None => "Starting debugger on the remote…".into(),
        };
        // This run's identity: every message the adapter stream produces carries
        // it, so a late event from a previous run can't land on this session.
        self.bump_debug_run();
        let run = self.debug_run;
        let live = self.debug_run_live.clone();

        // Preferred: spawn the debug adapter on clew-server (it must run where the
        // program does). Allocate its proc handle up front; the stream sets up the
        // proxy after it resolves the adapter binary.
        let proc = self.next_proc_id;
        self.next_proc_id += 1;
        let server_tx = self.server.tx().cloned();
        // The window's request counter: every request this run sends (the
        // proxied stdin, the kill) takes a fresh id from it.
        let next_id = self.next_req_id.clone();
        // The generic request/reply handle, for the remote flow's
        // launch-config fetch and adapter spawn.
        let ai = self.ai_client();
        // Every message this run produces answers to the project and the
        // transport it was started over, as well as to `run`.
        let stamp = self.transport_stamp();

        let stream = iced::stream::channel(
            64,
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                use iced::futures::SinkExt;
                use std::sync::atomic::Ordering;
                // A Stop during startup bumps the live run counter; each slow
                // step re-checks it so the startup actually CANCELS — killing
                // what it already spawned — instead of finishing invisibly
                // (Stop→Start used to leave two adapters and debuggees alive).
                let cancelled = |live: &std::sync::Arc<std::sync::atomic::AtomicU64>| {
                    live.load(Ordering::SeqCst) != run
                };
                // Resolve, spawn, and connect — two shapes of the same flow:
                //   local:  resolve on THIS machine (may probe xcrun/pip),
                //           spawn via clew-server (stdio) or locally (TCP);
                //   remote: everything about the adapter — its binary, the
                //           launch.json, the debuggee — lives on the remote
                //           host, so the server resolves AND spawns it
                //           (`SpawnAdapter`), and its reply carries the
                //           launch config built with remote paths. Nothing
                //           on this machine is read or probed.
                let (started, launch, addr) = if remote {
                    let Some(tx) = server_tx.clone() else {
                        let _ = output
                            .send(Message::Debug(DebugMsg::Failed {
                                stamp: stamp.clone(),
                                run,
                                error: "not connected to the remote server".into(),
                            }))
                            .await;
                        return;
                    };
                    // The launch config lives in the REMOTE .clew.
                    let text = match ai
                        .request(clew_protocol::Request::ReadState {
                            root: root.to_string_lossy().into_owned(),
                            rel: "launch.json".into(),
                        })
                        .await
                    {
                        Ok(clew_protocol::Event::StateContent { text: Some(t), .. }) => t,
                        Ok(clew_protocol::Event::StateContent { text: None, .. }) => {
                            let _ = output
                                .send(Message::Debug(DebugMsg::Failed {
                                    stamp: stamp.clone(),
                                    run,
                                    error: "Create .clew/launch.json in the REMOTE project \
                                            with {\"program\": \"path\", \"type\": \"...\"}"
                                        .into(),
                                }))
                                .await;
                            return;
                        }
                        // Why is part of the answer: a refused read (an
                        // unreadable or symlinked file) is not a missing one.
                        other => {
                            let why = match other {
                                Err(e) => e,
                                Ok(event) => format!(
                                    "unexpected reply: {}",
                                    crate::app::rpc::event_name(&event)
                                ),
                            };
                            let _ = output
                                .send(Message::Debug(DebugMsg::Failed {
                                    stamp: stamp.clone(),
                                    run,
                                    error: format!("could not read the remote launch.json: {why}"),
                                }))
                                .await;
                            return;
                        }
                    };
                    let cfg = match parse_launch_config(&root, &text) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            let _ = output
                                .send(Message::Debug(DebugMsg::Failed {
                                    stamp: stamp.clone(),
                                    run,
                                    error: e,
                                }))
                                .await;
                            return;
                        }
                    };
                    let Some(lang) = dap::Lang::detect(cfg.type_hint.as_deref(), &cfg.program)
                    else {
                        let _ = output
                            .send(Message::Debug(DebugMsg::Failed {
                                stamp: stamp.clone(),
                                run,
                                error: format!(
                                    "Unknown debug type {:?} in the remote launch.json",
                                    cfg.type_hint
                                ),
                            }))
                            .await;
                        return;
                    };
                    if cancelled(&live) {
                        return; // stopped before anything was spawned
                    }
                    // The bridge's client end holds the adapter from here on:
                    // every return below drops it, and the bridge then kills
                    // the process on the server (see "The teardown" below).
                    let (stdin, stdout, feed) = proxy_streams(&tx, &next_id, proc);
                    // Register the output feed before the adapter can answer.
                    let _ = output
                        .send(Message::Server(ServerMsg::RegisterProcFeed {
                            stamp: stamp.clone(),
                            proc,
                            feed,
                        }))
                        .await;
                    let launch = match ai
                        .request(clew_protocol::Request::SpawnAdapter {
                            proc,
                            lang: lang.slug().into(),
                            program: cfg.program.to_string_lossy().into_owned(),
                            args: cfg.args.clone(),
                        })
                        .await
                    {
                        Ok(clew_protocol::Event::AdapterSpawned { launch, .. }) => {
                            match serde_json::from_str::<serde_json::Value>(&launch) {
                                Ok(v) => v,
                                Err(e) => {
                                    let _ = output
                                        .send(Message::Debug(DebugMsg::Failed {
                                            stamp: stamp.clone(),
                                            run,
                                            error: format!("bad launch config: {e}"),
                                        }))
                                        .await;
                                    return;
                                }
                            }
                        }
                        Ok(other) => {
                            let _ = output
                                .send(Message::Debug(DebugMsg::Failed {
                                    stamp: stamp.clone(),
                                    run,
                                    error: format!("unexpected SpawnAdapter reply: {other:?}"),
                                }))
                                .await;
                            return;
                        }
                        // The server already retracted the proc and reported
                        // its exit on failure: the bridge's kill finds
                        // nothing there.
                        Err(e) => {
                            let _ = output
                                .send(Message::Debug(DebugMsg::Failed {
                                    stamp: stamp.clone(),
                                    run,
                                    error: e,
                                }))
                                .await;
                            return;
                        }
                    };
                    (dap::DapClient::connect(stdin, stdout).await, launch, None)
                } else {
                    let lang = lang.expect("local start_debug always detects the language");
                    // Resolve the adapter for this language (locates its
                    // binary + builds the launch arguments). On the blocking
                    // pool: it may run xcrun or probe a Python interpreter.
                    // It never installs anything.
                    let resolved = {
                        let (program, args, root, cwd) =
                            (program.clone(), args.clone(), root.clone(), cwd.clone());
                        tokio::task::spawn_blocking(move || {
                            dap::adapter::resolve(lang, &program, &args, &root, &cwd)
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("resolving the debug adapter failed: {e}")))
                    };
                    let adapter = match resolved {
                        Ok(dap::Resolved::Ready(a)) => a,
                        // The adapter must be installed first: that is the
                        // user's call, asked through the consent modal. This
                        // run ends here; an approved install starts a new one.
                        Ok(dap::Resolved::NeedsInstall(install)) => {
                            let _ = output
                                .send(Message::Debug(DebugMsg::NeedsInstall {
                                    stamp: stamp.clone(),
                                    run,
                                    install,
                                }))
                                .await;
                            return;
                        }
                        Err(e) => {
                            let _ = output
                                .send(Message::Debug(DebugMsg::Failed {
                                    stamp: stamp.clone(),
                                    run,
                                    error: e,
                                }))
                                .await;
                            return;
                        }
                    };
                    if cancelled(&live) {
                        return; // stopped before anything was spawned
                    }
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
                            let (stdin, stdout, feed) = proxy_transport(tx, &next_id, proc, spawn);
                            // Register the output feed before the adapter can answer.
                            let _ = output
                                .send(Message::Server(ServerMsg::RegisterProcFeed {
                                    stamp: stamp.clone(),
                                    proc,
                                    feed,
                                }))
                                .await;
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
                    // A TCP adapter's address is the one it announced once
                    // running (child sessions connect to it too).
                    let addr = started
                        .as_ref()
                        .ok()
                        .and_then(|(client, _)| client.tcp_addr());
                    (started, adapter.launch, addr)
                };
                // The teardown. Whatever this run spawned ends with it, and
                // once, through whoever holds it — not through a kill at
                // each exit below. A local adapter is reaped when its client's
                // actor ends, which is when the last handle to the client
                // goes. A server-proxied one is killed by its stdio bridge
                // when the bridge's client end closes (`proxy_streams`): the
                // moment this stream returns, while no client holds that end
                // yet; otherwise, when the client's actor ends. So each exit
                // only has to let go of what it holds. An explicit
                // `ProcessKill` at each exit came on top of the bridge's, and
                // killed the adapter twice.
                let (client, mut events) = match started {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = output
                            .send(Message::Debug(DebugMsg::Failed {
                                stamp: stamp.clone(),
                                run,
                                error: e,
                            }))
                            .await;
                        return;
                    }
                };
                if cancelled(&live) {
                    return; // stopped while the adapter was starting
                }
                // initialize() can hang forever on a wedged adapter, and the
                // run-counter checkpoints only run BETWEEN awaits — so race
                // it against the Stop signal and a hard timeout. Without
                // this, Stop during a hung initialize could never terminate
                // the startup or the process it had spawned.
                let initialized = {
                    let stopped = async {
                        while !cancelled(&live) {
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        }
                    };
                    tokio::select! {
                        r = client.initialize() => Some(r),
                        _ = stopped => None,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => {
                            Some(Err("no answer to initialize within 20s".into()))
                        }
                    }
                };
                let capabilities = match initialized {
                    None => {
                        return; // stopped during initialize
                    }
                    Some(Err(e)) => {
                        let _ = output
                            .send(Message::Debug(DebugMsg::Failed {
                                stamp: stamp.clone(),
                                run,
                                error: format!("initialize: {e}"),
                            }))
                            .await;
                        return;
                    }
                    Some(Ok(capabilities)) => capabilities,
                };
                // `supportsEvaluateForHovers` is the adapter's promise that an
                // evaluate for a data hover has no side effects (DAP spec).
                let hover_safe = dap::promises_hover_eval(&capabilities);
                // A Stop that landed between initialize completing and here
                // must win BEFORE the debuggee is launched: the event-loop
                // checkpoint below only runs at the next adapter event, by
                // which time the program would already be running.
                if cancelled(&live) {
                    return; // stopped right after initialize
                }
                // Hand the client to the App *before* launching, so it holds the
                // handle when the `initialized` event arrives (it sends breakpoints).
                let _ = output
                    .send(Message::Debug(DebugMsg::DapStarted {
                        stamp: stamp.clone(),
                        run,
                        client: client.clone(),
                        addr,
                        hover_safe,
                    }))
                    .await;
                // That send is an await on a bounded channel — a real yield
                // point, and a Stop processed during it must still win. While
                // the session is `Launching` the App holds no client (Stop is
                // only a counter bump), so this is the ONLY place that can
                // stop the debuggee before it exists.
                if cancelled(&live) {
                    let _ = client.disconnect().await;
                    return; // stopped between handing over the client and launching
                }
                client.launch(launch);
                loop {
                    // Race the adapter against the Stop flag: checking only on
                    // the next event meant a silent adapter left the debuggee
                    // running indefinitely after Stop.
                    let ev = tokio::select! {
                        ev = events.recv() => ev,
                        () = async {
                            while !cancelled(&live) {
                                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            }
                        } => None,
                    };
                    let Some(ev) = ev else { break };
                    if cancelled(&live) {
                        // Ask the adapter to terminate the debuggee, then let
                        // the adapter go (see the teardown note above):
                        // killing the adapter alone can orphan the program it
                        // launched.
                        let _ = client.disconnect().await;
                        return; // stopped: no final Terminated for a dead run
                    }
                    if output
                        .send(Message::Debug(DebugMsg::DapEvent {
                            stamp: stamp.clone(),
                            run,
                            event: ev,
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Left the loop because the flag fired rather than the adapter
                // closing: tear the run down the same way.
                if cancelled(&live) {
                    let _ = client.disconnect().await;
                    return;
                }
                // Adapter closed: make sure the session tears down.
                let _ = output
                    .send(Message::Debug(DebugMsg::DapEvent {
                        stamp: stamp.clone(),
                        run,
                        event: dap::DapEvent::Terminated,
                    }))
                    .await;
            },
        );
        Task::run(stream, |m| m)
    }

    /// The debug adapter a run needs is not installed. Nothing is installed
    /// from the Debug button: the user is asked first, through the same
    /// consent modal a language-server install uses, and only an approval runs
    /// the install ([`Self::on_debug_install_allowed`]).
    pub(crate) fn on_debug_needs_install(
        &mut self,
        run: u64,
        install: dap::AdapterInstall,
    ) -> Task<Message> {
        if run != self.debug_run {
            return Task::none();
        }
        self.debug.session = None;
        if self.proj.project.is_none() {
            return Task::none();
        }
        self.status = format!(
            "Debugging needs {} — waiting for your approval",
            install.name()
        );
        self.proj.pending_lsp_consent.offer(LspConsent {
            language: String::new(),
            server_name: install.name().to_string(),
            version: install.version().to_string(),
            provision: LspProvision::DebugAdapter {
                install,
                stamp: self.stamp(),
            },
            dest_dir: PathBuf::new(),
        });
        Task::none()
    }

    /// The user approved a debug-adapter install: run it off the UI thread.
    pub(crate) fn on_debug_install_allowed(
        &mut self,
        install: dap::AdapterInstall,
        stamp: Stamp,
    ) -> Task<Message> {
        self.status = format!("Installing {} {}…", install.name(), install.version());
        // Raised when the project it was consented in is left (the session
        // is dropped whole): the install stops between chunks, or its `pip`
        // is killed, instead of running on for a session that cannot start.
        let cancel = self.proj.adapter_install_cancel.flag();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || install.install_cancellable(&cancel))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            },
            move |result| {
                Message::Debug(DebugMsg::AdapterInstalled {
                    stamp: stamp.clone(),
                    result,
                })
            },
        )
    }

    /// A consented debug-adapter install finished. On success the session the
    /// user asked for starts — unless the project changed in the meantime:
    /// another project, or the same root opened again (possibly on another
    /// host, where the same path is a different project), is not the one the
    /// Debug button was pressed in.
    pub(crate) fn on_debug_adapter_installed(
        &mut self,
        stamp: Stamp,
        result: Result<(), String>,
    ) -> Task<Message> {
        match result {
            // Stopped because the project it was for was left (see
            // `on_debug_install_allowed`): not a failure of the install.
            Err(e) if !self.owns(&stamp) => {
                self.status =
                    format!("Debug adapter install stopped with the project it was for ({e})");
                Task::none()
            }
            Err(e) => {
                self.status = format!("Debug adapter install failed: {e}");
                Task::none()
            }
            // The install is global and reported whatever the project; only
            // starting the session belongs to the project it was asked in.
            Ok(()) if self.owns(&stamp) => self.start_debug(),
            Ok(()) => {
                self.status = "Debug adapter installed".into();
                Task::none()
            }
        }
    }

    pub(crate) fn on_debug_stop(&mut self) -> Task<Message> {
        self.status = "Debugger stopped".into();
        // The teardown itself is shared with the project switch, which used to
        // carry its own copy of it (see `stop_debug_session`).
        self.stop_debug_session()
    }

    /// Tear down the work this window owns, just before the shell drops its
    /// `App` (see [`crate::shell`]'s `Shell::Closed`).
    ///
    /// The debug session is the part that needs a hook: the startup stream
    /// holding the adapter handle runs on the daemon's runtime, not on the
    /// window, so it survives the `App` and keeps the adapter (and the
    /// debuggee) alive. Stopping it is exactly what the Stop button does — the
    /// run identity moves on, the stream sees that at its next 150 ms
    /// checkpoint and disconnects + reaps what it spawned — which is what
    /// [`Self::drop_project_work`] does, through [`Self::stop_debug_session`].
    /// Unconditionally, even with no `debug.session`: the run counter, not
    /// the session, is the handle on the startup stream.
    ///
    /// The rest of the project's work goes the way it goes on a project
    /// switch ([`Self::drop_project_work`]): the Explain pass and the LSP
    /// call-graph refine are iced `Handle`s, which do not abort when dropped
    /// — Explain kept issuing (billable) LLM calls for a window that was
    /// gone, and the refine kept clones of the language-server clients
    /// alive — and the Ask streams are stopped where they run.
    ///
    /// The window's clew-server is released with it: nothing here wants one
    /// any more, so the subscription goes, the server sees EOF, gets its
    /// grace to reap what it started, and a transport ending now is not
    /// restarted (see `App::quitting`). Bookmark, note and walkthrough edits
    /// the journal still holds are sent first, with the requests the
    /// transport flushes as it goes; those that did not go — no transport, a
    /// full queue, an earlier edit they wait for — are lost with the window,
    /// those sent that the host never confirmed may be, and those given up
    /// are: stderr says so for each, in the words the question used. The
    /// user was asked about them before the window went, while it could
    /// still be kept open (the shell's `Shell::CloseRequested`).
    pub(crate) fn on_window_closed(&mut self) -> Closing {
        // Said as the question says it, each for what it is: the edits the
        // host could not save were logged as never sent, and the ones given
        // up not at all.
        if let Some(given_up) = self.unsaved_edits().filter(|u| u.given_up() > 0) {
            eprintln!("[clew] {}", given_up.lost_sentence(true));
        }
        if let Some(lost) = self.take_unsendable_edits() {
            eprintln!("[clew] lost with the window: {}", lost.line());
        }
        if let Some(sent) = self.unconfirmed_edits() {
            eprintln!("[clew] may be lost with the window: {}", sent.line());
        }
        self.quitting = true;
        // The update download is this window's too, and iced keeps draining a
        // closed window's streams: left running, it fetched a whole image for
        // nobody to install. Its run is superseded too, as Cancel does, so a
        // result already on its way can never start an install once the
        // window is closing: the shell drops this `App` as it closes it, so
        // such a result normally finds no window at all, and one that still
        // reaches this `App` no longer matches the run and has its image
        // discarded (`on_update_downloaded`).
        self.abort_update_download();
        self.update.generation += 1;
        Closing {
            teardown: self.drop_project_work(),
        }
    }

    /// End the debug session: retire this run's identity, forget what the
    /// dying adapter said about the user's breakpoints, and disconnect it.
    ///
    /// The single implementation of the teardown, shared by the Stop button
    /// ([`Self::on_debug_stop`], which adds the status line) and by
    /// `drop_project_work`, which leaves a project the debuggee belongs to.
    /// Those two were line-for-line copies of each other, which is how a fix to
    /// one could have landed in only one of them; the status line is the only
    /// difference left.
    pub(crate) fn stop_debug_session(&mut self) -> Task<Message> {
        // End this run's identity: the adapter stream keeps draining after the
        // disconnect and its late events (a final Terminated, a stop
        // inspection) must not land on the next session. This also CANCELS a
        // startup still in flight (no client yet to disconnect): the stream
        // checks the live counter and kills what it spawned.
        self.bump_debug_run();
        self.forget_adapter_verdicts();
        // The next adapter answers `initialize` for itself.
        self.debug.hover_safe = false;
        match self.debug.session.take().and_then(|s| s.client) {
            Some(client) => Task::perform(
                async move {
                    let _ = client.disconnect().await;
                },
                |()| Message::Noop,
            ),
            None => Task::none(),
        }
    }

    /// Drop everything the adapter told us about the user's breakpoints,
    /// keeping only `condition`, which is the user's own input and not a
    /// verdict.
    ///
    /// Called from [`Self::stop_debug_session`], where the adapter is being
    /// dropped and its answers can no longer be refreshed or corrected.
    /// `Bp::verified` says `None` means "nobody has answered" and `Some(false)`
    /// means "this will never fire", and the gutter draws the second hollow —
    /// so without this a line the last adapter could not bind keeps being drawn
    /// as dead after Stop, and through a later start that dies before
    /// `Initialized` or whose `setBreakpoints` errors, with no adapter alive to
    /// assert it. `adapter_id` is cleared for a sharper reason: adapters hand
    /// out small integer handles from zero, so a handle left over from a dead
    /// session can collide with a new adapter's and make
    /// `on_breakpoint_changed`'s first-match scan relabel the wrong line.
    ///
    /// This does NOT make every stale verdict impossible, and the teardown is
    /// the only caller on purpose — the other two `bump_debug_run` sites are
    /// not adapter deaths this can speak for. A verdict recorded mid-run stands
    /// until the session is stopped: `drop_connection_state` marks the session
    /// `Terminated` without taking it, so between a lost transport and the Stop
    /// that clears it the gutter still shows the last adapter's rings.
    pub(crate) fn forget_adapter_verdicts(&mut self) {
        for file in self.debug.breakpoints.values_mut() {
            for bp in file.values_mut() {
                bp.verified = None;
                bp.bound_line = None;
                bp.adapter_id = None;
            }
        }
    }

    /// Advance the debug-run identity (see `debug_run_live`): late messages
    /// from the previous run are dropped, and an in-flight startup stream
    /// cancels itself at its next checkpoint.
    pub(crate) fn bump_debug_run(&mut self) {
        self.debug_run += 1;
        self.debug_run_live
            .store(self.debug_run, std::sync::atomic::Ordering::SeqCst);
        self.bump_debug_stop();
    }

    /// Leave the current stop (a new stop, a step/continue, or the run ending):
    /// anything still being fetched for the stop we are leaving is now stale.
    pub(crate) fn bump_debug_stop(&mut self) {
        self.debug_stop += 1;
    }

    /// Whether a DAP result tagged `(run, stop)` still describes where the
    /// program is paused NOW. The run alone is not enough: a stack/scopes or
    /// watch reply for the first stop of a run can arrive after the user
    /// pressed Continue, or after the program stopped again, and painting it
    /// would jump the editor back and show a stack and variables that no
    /// longer exist.
    pub(crate) fn owns_debug_stop(&self, run: u64, stop: u64) -> bool {
        run == self.debug_run && stop == self.debug_stop
    }

    /// Fold a DAP adapter event into the session state. `run` names the session
    /// the event came from; anything from a previous run is dropped (a stopped
    /// adapter's stream drains asynchronously and always ends with a final
    /// `Terminated`, which must not kill the next session).
    pub(crate) fn on_dap_event(&mut self, run: u64, ev: dap::DapEvent) -> Task<Message> {
        let stamp = self.transport_stamp();
        if run != self.debug_run {
            return Task::none();
        }
        // Handled before the `session` borrow below, because it writes to
        // `self.debug.breakpoints` and the two cannot be borrowed at once.
        if let dap::DapEvent::BreakpointChanged {
            id,
            verified,
            line,
            message,
        } = &ev
        {
            return self.on_breakpoint_changed(*id, *verified, *line, message.clone());
        }
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
                let bp_run = self.debug_run;
                Task::perform(
                    async move {
                        let mut answers = Vec::with_capacity(bps.len());
                        for (file, lines) in bps {
                            answers
                                .push((file.clone(), client.set_breakpoints(&file, &lines).await));
                        }
                        let _ = client.configuration_done().await;
                        answers
                    },
                    move |answers| {
                        Message::Debug(DebugMsg::DapBreakpointsAnswered {
                            stamp: stamp.clone(),
                            run: bp_run,
                            answers,
                        })
                    },
                )
            }
            dap::DapEvent::Stopped(s) => {
                session.status = DebugStatus::Stopped;
                session.thread_id = s.thread_id;
                self.debug.pending_reason = s.reason.clone();
                let Some(client) = session.client.clone() else {
                    return Task::none();
                };
                let tid = s.thread_id.unwrap_or(0);
                self.status = format!("Stopped: {}", s.reason);
                // Load the stack, then the top frame's scopes + variables.
                let run = self.debug_run;
                // This is a new stop: whatever is still loading for the previous
                // one is stale, and this fetch is stamped with the new identity.
                self.bump_debug_stop();
                let stop = self.debug_stop;
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
                    move |(frames, scopes)| {
                        Message::Debug(DebugMsg::DapStopInspected {
                            stamp: stamp.clone(),
                            run,
                            stop,
                            frames,
                            scopes,
                        })
                    },
                )
            }
            dap::DapEvent::Continued { .. } => {
                session.status = DebugStatus::Running;
                session.current = None;
                session.frames.clear();
                session.scopes.clear();
                session.watches.clear();
                // Clearing alone is only a PRIOR wipe: without leaving the stop
                // behind, an inspection still in flight for it would arrive and
                // fill the panel back in while the program is running.
                self.bump_debug_stop();
                Task::none()
            }
            dap::DapEvent::Output(o) => {
                push_debug_output(session, o.category, o.text);
                Task::none()
            }
            dap::DapEvent::Exited { code } => {
                push_debug_output(
                    session,
                    "console".into(),
                    format!("Process exited with code {code}\n"),
                );
                session.status = DebugStatus::Terminated;
                session.current = None;
                // The program is gone: an inspection still loading for the last
                // stop must not resurrect a location in a dead process.
                self.bump_debug_stop();
                Task::none()
            }
            dap::DapEvent::Terminated => {
                session.status = DebugStatus::Terminated;
                session.current = None;
                session.frames.clear();
                session.scopes.clear();
                self.bump_debug_stop();
                Task::none()
            }
            dap::DapEvent::StartDebugging(config) => {
                // js-debug: open a child session on the same adapter for the real
                // target, then drive its handshake (it owns the breakpoints/stack).
                let Some(addr) = session.addr else {
                    return Task::none();
                };
                let run = self.debug_run;
                let stream = iced::stream::channel(
                    64,
                    move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                        use iced::futures::SinkExt;
                        // A child that cannot start is said so, not left
                        // silent (`ChildFailed`). Whether the run fails with
                        // it is the App's call: only when no child of the run
                        // has started, which makes this one its real target
                        // (`on_debug_child_failed`).
                        let (client, mut events) =
                            match dap::DapClient::connect_tcp_addr(addr).await {
                                Ok(pair) => pair,
                                Err(e) => {
                                    let _ = output
                                        .send(Message::Debug(DebugMsg::ChildFailed {
                                            stamp: stamp.clone(),
                                            run,
                                            error: format!("connect: {e}"),
                                        }))
                                        .await;
                                    return;
                                }
                            };
                        // Its capabilities are its own: the hovers go to it,
                        // so its promise is the one that counts
                        // (`DapChildStarted`).
                        let capabilities = match client.initialize().await {
                            Ok(capabilities) => capabilities,
                            Err(e) => {
                                let _ = output
                                    .send(Message::Debug(DebugMsg::ChildFailed {
                                        stamp: stamp.clone(),
                                        run,
                                        error: format!("initialize: {e}"),
                                    }))
                                    .await;
                                // A child spawns no process (the adapter is
                                // the parent's); what it holds is this
                                // connection. End its session there, then
                                // let the client go, which closes it.
                                let _ = client.disconnect().await;
                                return;
                            }
                        };
                        let _ = output
                            .send(Message::Debug(DebugMsg::DapChildStarted {
                                stamp: stamp.clone(),
                                run,
                                client: client.clone(),
                                hover_safe: dap::promises_hover_eval(&capabilities),
                            }))
                            .await;
                        client.launch(config);
                        while let Some(ev) = events.recv().await {
                            if output
                                .send(Message::Debug(DebugMsg::DapEvent {
                                    stamp: stamp.clone(),
                                    run,
                                    event: ev,
                                }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                );
                Task::run(stream, |m| m)
            }
            // Taken above, before the session borrow.
            dap::DapEvent::BreakpointChanged { .. } => Task::none(),
            dap::DapEvent::Other(_) => Task::none(),
        }
    }

    /// Run `run`'s own start failed — only its startup stream sends this — so
    /// the run is over, and torn down like a Stop: its identity retired, the
    /// adapter's verdicts forgotten, the session taken with any client it
    /// holds.
    pub(crate) fn on_debug_failed(&mut self, run: u64, error: String) -> Task<Message> {
        if run != self.debug_run {
            return Task::none();
        }
        let teardown = self.stop_debug_session();
        self.status = format!("Debug failed: {error}");
        teardown
    }

    /// A js-debug child session of run `run` could not start. That child is
    /// over — its stream ends the connection it made, if any.
    ///
    /// Before any child of the run has started, the one that failed was the
    /// run's real target (js-debug's first child is the program itself): the
    /// run fails with it, as a failed start does. Only reported, it left the
    /// parent alone under a green "running" badge, with nothing behind it and
    /// a status line the next message overwrites.
    ///
    /// Once a child has started, nothing else is over: the parent session,
    /// which holds the adapter and answers the Stop, and the other children
    /// run on, so this only says it, in the status line and the Debug console.
    /// Ending the run then took a healthy main target down whenever a later
    /// child failed.
    pub(crate) fn on_debug_child_failed(&mut self, run: u64, error: String) -> Task<Message> {
        if run != self.debug_run {
            return Task::none();
        }
        if self.debug.child_started != Some(run) {
            return self.on_debug_failed(
                run,
                format!("the debug target's session did not start — {error}"),
            );
        }
        let Some(session) = self.debug.session.as_mut() else {
            return Task::none();
        };
        push_debug_output(
            session,
            "console".into(),
            format!("[clew] a child debug session failed to start — {error}\n"),
        );
        self.status = format!("Child debug session failed — {error}");
        Task::none()
    }

    pub(crate) fn on_dap_stop_inspected(
        &mut self,
        frames: Vec<dap::StackFrame>,
        scopes: Vec<DebugScope>,
    ) -> Task<Message> {
        // Jump to the innermost frame that has source, and highlight it.
        let (target, fname) = {
            let Some(session) = self.debug.session.as_mut() else {
                return Task::none();
            };
            session.frames = frames;
            session.scopes = scopes;
            // The trace: this stop, with its stack, up to the cap.
            if self.debug.trace.len() < MAX_TRACE_STOPS {
                self.debug.trace.push(TraceStop {
                    reason: std::mem::take(&mut self.debug.pending_reason),
                    frames: session
                        .frames
                        .iter()
                        .map(|f| TraceFrame {
                            name: f.name.clone(),
                            path: f.path.clone(),
                            line: f.line,
                        })
                        .collect(),
                });
                self.debug.trace_rev += 1;
            } else if !self.debug.trace_cut {
                self.debug.trace_cut = true;
                self.debug.trace_rev += 1;
            }
            let t = session
                .frames
                .iter()
                .find_map(|f| f.path.clone().map(|p| (p, f.line)));
            if let Some((path, line)) = &t {
                session.current = Some((path.clone(), *line));
            }
            let fname = session.frames.first().map(|f| short_frame_name(&f.name));
            (t, fname)
        };
        // Fuse into the reading trail: when execution enters a NEW
        // function, record one entry (labelled with the function name) so
        // the debug run becomes a navigable path in the TRAIL tab.
        let mut saved = Task::none();
        if let (Some(fname), Some((path, line))) = (&fname, &target)
            && self.debug.last_fn.as_ref() != Some(fname)
        {
            self.debug.last_fn = Some(fname.clone());
            self.proj.history.push(
                Loc {
                    path: path.clone(),
                    line: Some(*line),
                },
                Some(fname.clone()),
            );
            saved = self.save_history();
        }
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug;
        match target {
            Some((path, line)) => Task::batch([
                saved,
                self.open_file(path, Some(line), false),
                self.eval_watches(),
            ]),
            None => Task::batch([saved, self.eval_watches()]),
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
        let stamp = self.transport_stamp();
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
        // Stamped so a reply from a session the user has already stopped cannot
        // relabel the breakpoints of the next one, the same generation guard
        // `on_dap_event` and `eval_watches` use.
        let run = self.debug_run;
        Task::perform(
            async move {
                let answer = client.set_breakpoints(&p, &lines).await;
                vec![(p, answer)]
            },
            move |answers| {
                Message::Debug(DebugMsg::DapBreakpointsAnswered {
                    stamp: stamp.clone(),
                    run,
                    answers,
                })
            },
        )
    }

    /// Record what the adapter said about a file's breakpoints.
    ///
    /// Only lines the adapter actually answered for are touched: a
    /// non-conforming adapter that returns fewer entries than we sent leaves
    /// the rest at "unknown", which is honest, rather than shifting somebody
    /// else's verdict onto them (`DapClient::set_breakpoints` pairs by `zip`
    /// for the same reason).
    pub(crate) fn on_dap_breakpoints_answered(
        &mut self,
        run: u64,
        answers: Vec<(PathBuf, Result<Vec<dap::Breakpoint>, String>)>,
    ) -> Task<Message> {
        if run != self.debug_run {
            return Task::none();
        }
        let mut refused = 0usize;
        let mut moved: Option<(String, usize, usize)> = None;
        for (path, answer) in answers {
            let list = match answer {
                Ok(list) => list,
                Err(e) => {
                    // The request failed or timed out. `let _ =` used to hide
                    // this too: every breakpoint in the file stays "unknown",
                    // so say so instead of leaving a silently dead gutter.
                    self.status = format!("Breakpoints in {} not set: {e}", self.rel_of(&path));
                    continue;
                }
            };
            // `rel_of` borrows self, so the relocation is noted here and named
            // after the map borrow below has ended.
            let mut moved_here = None;
            let Some(file) = self.debug.breakpoints.get_mut(&path) else {
                continue;
            };
            for bp in list {
                let Some(entry) = file.get_mut(&bp.requested_line) else {
                    continue;
                };
                entry.verified = Some(bp.verified);
                entry.bound_line = bp.relocated_to();
                entry.adapter_id = bp.id;
                if !bp.verified {
                    refused += 1;
                } else if let Some(to) = bp.relocated_to() {
                    moved_here = Some((bp.requested_line, to));
                }
            }
            if let Some((from, to)) = moved_here {
                moved = Some((self.rel_of(&path), from, to));
            }
        }
        // One line of status, preferring the refusals: a breakpoint that will
        // never fire is worth more than one that merely slid a line.
        if refused > 0 {
            self.status = format!(
                "{refused} breakpoint{} could not be set (drawn hollow)",
                if refused == 1 { "" } else { "s" }
            );
        } else if let Some((rel, from, to)) = moved {
            self.status = format!("Breakpoint {rel}:{from} bound to line {to}");
        }
        Task::none()
    }

    /// Apply a `breakpoint` event: the adapter revising an answer it already
    /// gave. Matched by the adapter's own handle, which is all the event
    /// carries — see [`dap::DapEvent::BreakpointChanged`].
    fn on_breakpoint_changed(
        &mut self,
        id: Option<i64>,
        verified: bool,
        line: Option<usize>,
        message: Option<String>,
    ) -> Task<Message> {
        // Without a handle there is nothing to match on. Guessing by line would
        // relabel a breakpoint the event was not about.
        let Some(id) = id else {
            return Task::none();
        };
        for file in self.debug.breakpoints.values_mut() {
            for (requested, bp) in file.iter_mut() {
                if bp.adapter_id != Some(id) {
                    continue;
                }
                let was = bp.verified;
                bp.verified = Some(verified);
                bp.bound_line = line.filter(|l| verified && *l != *requested);
                if was != Some(verified) {
                    self.status = match (verified, message) {
                        (true, _) => format!("Breakpoint at line {requested} is now live"),
                        (false, Some(m)) => format!("Breakpoint at line {requested}: {m}"),
                        (false, None) => format!("Breakpoint at line {requested} is not live"),
                    };
                }
                return Task::none();
            }
        }
        Task::none()
    }

    /// Re-evaluate all watch expressions in the current frame (on each stop, or
    /// when a watch is added). No-op unless paused with watches set.
    pub(crate) fn eval_watches(&self) -> Task<Message> {
        let stamp = self.transport_stamp();
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
        let run = self.debug_run;
        // The values are read in THIS frame: a reply that outlives the stop is
        // a reading of variables the program has already moved past.
        let stop = self.debug_stop;
        Task::perform(
            async move {
                let mut out = Vec::with_capacity(exprs.len());
                for e in exprs {
                    let v = client
                        .evaluate(&e, frame_id, dap::EvalContext::Watch)
                        .await
                        .unwrap_or_else(|err| format!("⚠ {err}"));
                    out.push((e, v));
                }
                out
            },
            move |vals| {
                Message::Debug(DebugMsg::WatchesEvaluated {
                    stamp: stamp.clone(),
                    run,
                    stop,
                    vals,
                })
            },
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
        // The user has left this stop (continue or step). Anything still being
        // fetched for it would otherwise land as a "paused here" that lies.
        self.bump_debug_stop();
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

    pub(crate) fn on_breakpoint_toggle(&mut self, path: PathBuf, line: usize) -> Task<Message> {
        let map = self.debug.breakpoints.entry(path.clone()).or_default();
        if map.remove(&line).is_none() {
            map.insert(line, Bp::default());
        }
        if map.is_empty() {
            self.debug.breakpoints.remove(&path);
        }
        self.push_breakpoints(&path)
    }

    pub(crate) fn on_toggle_breakpoint_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        let Some(abs) = self
            .proj
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .map(|v| v.abs.clone())
        else {
            return Task::none();
        };
        // menu.line is 0-based; breakpoints are 1-based.
        self.update(Message::Debug(DebugMsg::BreakpointToggle {
            path: abs,
            line: menu.line + 1,
        }))
    }

    pub(crate) fn on_conditional_breakpoint_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        let Some(abs) = self
            .proj
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .map(|v| v.abs.clone())
        else {
            return Task::none();
        };
        let line = menu.line + 1;
        // Pre-fill with any existing condition on this line.
        let existing = self
            .debug
            .breakpoints
            .get(&abs)
            .and_then(|m| m.get(&line))
            .and_then(|bp| bp.condition.clone())
            .unwrap_or_default();
        self.proj.bp_cond_edit = Some((abs, line, existing));
        // The condition editor's input takes the keyboard.
        self.code_focused = false;
        operation::focus(ui::bp_condition_input_id())
    }

    pub(crate) fn on_bp_condition_set(&mut self) -> Task<Message> {
        let Some((path, line, draft)) = self.proj.bp_cond_edit.take() else {
            return Task::none();
        };
        let cond = draft.trim();
        // The adapter's previous answer (if any) does not carry over: the
        // condition changes what we are asking for, and `push_breakpoints`
        // below asks again. Until that reply lands the state is "unknown".
        let bp = Bp {
            condition: (!cond.is_empty()).then(|| cond.to_string()),
            ..Bp::default()
        };
        self.debug
            .breakpoints
            .entry(path.clone())
            .or_default()
            .insert(line, bp);
        self.status = "Conditional breakpoint set".into();
        self.push_breakpoints(&path)
    }

    /// Remove a watch expression by identity. The watch list and the last
    /// stop's evaluated `(expression, value)` pairs are two lists that are not
    /// index-aligned — the values are whatever the last evaluation returned, a
    /// snapshot taken before any watch added since — so removing "entry `i`"
    /// from both deleted a DIFFERENT expression's value from the second.
    pub(crate) fn remove_watch(&mut self, expr: &str) -> Task<Message> {
        if let Some(pos) = self.debug.watches.iter().position(|w| w == expr) {
            self.debug.watches.remove(pos);
        }
        if let Some(s) = self.debug.session.as_mut() {
            s.watches.retain(|(e, _)| e != expr);
        }
        Task::none()
    }
}

impl App {
    /// Handle a [`DebugMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_debug(&mut self, message: DebugMsg) -> Task<Message> {
        match message {
            DebugMsg::Start => self.start_debug(),
            DebugMsg::DapStarted {
                run,
                client,
                addr,
                hover_safe,
                ..
            } => {
                if run == self.debug_run
                    && let Some(session) = self.debug.session.as_mut()
                {
                    session.client = Some(client);
                    session.addr = addr;
                    session.status = DebugStatus::Running;
                    self.debug.hover_safe = hover_safe;
                    self.status = "Debugger running…".into();
                }
                Task::none()
            }
            DebugMsg::DapChildStarted {
                run,
                client,
                hover_safe,
                ..
            } => {
                // js-debug's child session owns the real target: make it
                // active, and with it the promise ITS `initialize` made about
                // hover evaluations — every hover now goes to the child. The
                // run has its target now: a later child failing is only that
                // child (`on_debug_child_failed`).
                if run == self.debug_run
                    && let Some(session) = self.debug.session.as_mut()
                {
                    session.client = Some(client);
                    self.debug.hover_safe = hover_safe;
                    self.debug.child_started = Some(run);
                }
                Task::none()
            }
            DebugMsg::DapEvent { run, event, .. } => self.on_dap_event(run, event),
            DebugMsg::DapStopInspected {
                run,
                stop,
                frames,
                scopes,
                ..
            } => {
                // A late inspection — from a previous run, or from a stop this
                // run has already left — must not overwrite the current frames
                // or jump the editor back to the old stop location.
                if !self.owns_debug_stop(run, stop) {
                    return Task::none();
                }
                self.on_dap_stop_inspected(frames, scopes)
            }
            DebugMsg::Control(cmd) => self.debug_control(cmd),
            DebugMsg::Stop => self.on_debug_stop(),
            DebugMsg::BreakpointToggle { path, line } => self.on_breakpoint_toggle(path, line),
            DebugMsg::Failed { run, error, .. } => self.on_debug_failed(run, error),
            DebugMsg::ChildFailed { run, error, .. } => self.on_debug_child_failed(run, error),
            DebugMsg::NeedsInstall { run, install, .. } => {
                self.on_debug_needs_install(run, install)
            }
            DebugMsg::AdapterInstalled { stamp, result } => {
                self.on_debug_adapter_installed(stamp, result)
            }
            DebugMsg::ToggleBreakpointFromMenu => self.on_toggle_breakpoint_from_menu(),
            DebugMsg::ConditionalBreakpointFromMenu => self.on_conditional_breakpoint_from_menu(),
            DebugMsg::BpConditionInput(s) => {
                if let Some((_, _, draft)) = &mut self.proj.bp_cond_edit {
                    *draft = s;
                }
                Task::none()
            }
            DebugMsg::BpConditionSet => self.on_bp_condition_set(),
            DebugMsg::BpConditionCancel => {
                self.proj.bp_cond_edit = None;
                Task::none()
            }
            DebugMsg::WatchInput(s) => {
                self.debug.watch_input = s;
                Task::none()
            }
            DebugMsg::WatchAdd => {
                let expr = self.debug.watch_input.trim().to_string();
                if expr.is_empty() {
                    return Task::none();
                }
                self.debug.watches.push(expr);
                self.debug.watch_input.clear();
                self.eval_watches()
            }
            DebugMsg::WatchRemoveExpr(expr) => self.remove_watch(&expr),
            DebugMsg::ToggleHoverEval => {
                self.debug_hover_eval = !self.debug_hover_eval;
                self.status = if self.debug_hover_eval {
                    "Hovering a name while paused now evaluates it in the program".into()
                } else {
                    "Hover evaluation in the debuggee is off".into()
                };
                Task::none()
            }
            DebugMsg::WatchesEvaluated {
                run, stop, vals, ..
            } => {
                if self.owns_debug_stop(run, stop)
                    && let Some(s) = self.debug.session.as_mut()
                {
                    s.watches = vals;
                }
                Task::none()
            }
            DebugMsg::DapBreakpointsAnswered { run, answers, .. } => {
                self.on_dap_breakpoints_answered(run, answers)
            }
        }
    }
}

/// What closing a window leaves to the shell ([`App::on_window_closed`]).
pub(crate) struct Closing {
    /// The teardown the window's work still needs, run on the daemon's
    /// runtime.
    pub(crate) teardown: Task<Message>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_session() -> DebugSession {
        DebugSession {
            client: None,
            status: DebugStatus::Running,
            thread_id: None,
            frames: Vec::new(),
            scopes: Vec::new(),
            watches: Vec::new(),
            output: Vec::new(),
            current: None,
            program: PathBuf::from("/tmp/prog"),
            args: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            addr: None,
        }
    }

    /// The debug panel's tail used to cap the CHUNK COUNT only. A debuggee
    /// whose writes come back as few-but-large output events therefore kept
    /// 500 chunks of arbitrary size, pinning hundreds of megabytes in a panel
    /// that claimed to be bounded. Both caps have to hold at once: bytes for
    /// big chunks, count for a flood of small ones.
    #[test]
    fn retained_debug_output_holds_both_the_byte_cap_and_the_chunk_cap() {
        let mut big = blank_session();
        // 400 chunks of 64 KiB — the largest a chunk can be once the client
        // truncates it — is ~25 MiB while staying under the chunk cap, so
        // only the byte cap can trim this.
        for i in 0..400 {
            push_debug_output(
                &mut big,
                "stdout".into(),
                format!("{i}{}", "x".repeat(64 * 1024)),
            );
        }
        let bytes: usize = big.output.iter().map(|(c, t)| c.len() + t.len()).sum();
        assert!(
            bytes <= DEBUG_OUTPUT_MAX_BYTES,
            "retained {bytes} bytes, over the {DEBUG_OUTPUT_MAX_BYTES} cap"
        );
        assert!(big.output.len() < 400, "nothing was trimmed");
        // Trimming is oldest-first, so the newest output is what survives.
        assert!(big.output.last().unwrap().1.starts_with("399"));

        let mut chatty = blank_session();
        for i in 0..5_000 {
            push_debug_output(&mut chatty, "stdout".into(), format!("line {i}\n"));
        }
        assert_eq!(chatty.output.len(), DEBUG_OUTPUT_MAX_CHUNKS);
        assert_eq!(chatty.output.last().unwrap().1, "line 4999\n");
    }
}
