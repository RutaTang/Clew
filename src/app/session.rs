//! Debug sessions (DAP), reading-session persistence, file open/load, key handling, and code-view highlight computation.

use crate::app::prelude::*;
use crate::*;

/// The per-project state files a REMOTE project loads over the protocol.
/// Order does not matter; each is requested and applied independently.
pub(crate) const REMOTE_STATE_FILES: &[&str] = &[
    "history.json",
    "bookmarks.json",
    "notes.json",
    "reading.toml",
    walkthrough::LIBRARY_REL,
];

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
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
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
            port: None,
        });
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug; // reveal the debug panel
        self.debug.last_fn = None;
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
        let server_tx = self.server_tx.clone();
        // The generic request/reply handle, for the remote flow's
        // launch-config fetch and adapter spawn.
        let ai = self.ai_client();

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
                let (started, launch, port, proxied) = if remote {
                    let Some(tx) = server_tx.clone() else {
                        let _ = output
                            .send(Message::DebugFailed {
                                run,
                                error: "not connected to the remote server".into(),
                            })
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
                                .send(Message::DebugFailed {
                                    run,
                                    error: "Create .clew/launch.json in the REMOTE project \
                                            with {\"program\": \"path\", \"type\": \"...\"}"
                                        .into(),
                                })
                                .await;
                            return;
                        }
                        Ok(_) | Err(_) => {
                            let _ = output
                                .send(Message::DebugFailed {
                                    run,
                                    error: "could not read the remote launch.json".into(),
                                })
                                .await;
                            return;
                        }
                    };
                    let cfg = match parse_launch_config(&root, &text) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            let _ = output.send(Message::DebugFailed { run, error: e }).await;
                            return;
                        }
                    };
                    let Some(lang) = dap::Lang::detect(cfg.type_hint.as_deref(), &cfg.program)
                    else {
                        let _ = output
                            .send(Message::DebugFailed {
                                run,
                                error: format!(
                                    "Unknown debug type {:?} in the remote launch.json",
                                    cfg.type_hint
                                ),
                            })
                            .await;
                        return;
                    };
                    if cancelled(&live) {
                        return; // stopped before anything was spawned
                    }
                    let (stdin, stdout, feed) = proxy_streams(&tx, proc);
                    // Register the output feed before the adapter can answer.
                    let _ = output.send(Message::RegisterProcFeed { proc, feed }).await;
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
                                        .send(Message::DebugFailed {
                                            run,
                                            error: format!("bad launch config: {e}"),
                                        })
                                        .await;
                                    return;
                                }
                            }
                        }
                        Ok(other) => {
                            let _ = output
                                .send(Message::DebugFailed {
                                    run,
                                    error: format!("unexpected SpawnAdapter reply: {other:?}"),
                                })
                                .await;
                            return;
                        }
                        // The server already retracted the proc and reported
                        // its exit on failure.
                        Err(e) => {
                            let _ = output.send(Message::DebugFailed { run, error: e }).await;
                            return;
                        }
                    };
                    (
                        dap::DapClient::connect(stdin, stdout).await,
                        launch,
                        None,
                        true,
                    )
                } else {
                    let lang = lang.expect("local start_debug always detects the language");
                    // Resolve the adapter for this language (locates its
                    // binary + builds the launch arguments). Off the UI
                    // thread as it may spawn xcrun/pip.
                    let adapter = match dap::adapter::resolve(lang, &program, &args, &cwd) {
                        Ok(a) => a,
                        Err(e) => {
                            let _ = output.send(Message::DebugFailed { run, error: e }).await;
                            return;
                        }
                    };
                    let port = match adapter.transport {
                        dap::client::Transport::Tcp(p) => Some(p),
                        dap::client::Transport::Stdio => None,
                    };
                    if cancelled(&live) {
                        return; // stopped before anything was spawned
                    }
                    // Stdio adapters (lldb-dap) run on clew-server, proxied; TCP adapters
                    // or a missing server fall back to a local spawn.
                    let proxied = matches!(
                        (&adapter.transport, &server_tx),
                        (dap::client::Transport::Stdio, Some(_))
                    );
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
                    (started, adapter.launch, port, proxied)
                };
                // Kill whatever this run spawned. Dropping the local client
                // closes its actor, which kills the child; a server-proxied
                // adapter needs an explicit ProcessKill.
                let kill = |server_tx: &Option<
                    tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>,
                >| {
                    if proxied && let Some(tx) = server_tx {
                        let _ = tx.send(clew_protocol::ClientMessage {
                            id: 0,
                            request: clew_protocol::Request::ProcessKill { proc },
                        });
                    }
                };
                let (client, mut events) = match started {
                    Ok(pair) => pair,
                    Err(e) => {
                        kill(&server_tx);
                        let _ = output.send(Message::DebugFailed { run, error: e }).await;
                        return;
                    }
                };
                if cancelled(&live) {
                    kill(&server_tx);
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
                match initialized {
                    None => {
                        kill(&server_tx);
                        return; // stopped during initialize
                    }
                    Some(Err(e)) => {
                        kill(&server_tx);
                        let _ = output
                            .send(Message::DebugFailed {
                                run,
                                error: format!("initialize: {e}"),
                            })
                            .await;
                        return;
                    }
                    Some(Ok(_)) => {}
                }
                // A Stop that landed between initialize completing and here
                // must win BEFORE the debuggee is launched: the event-loop
                // checkpoint below only runs at the next adapter event, by
                // which time the program would already be running.
                if cancelled(&live) {
                    kill(&server_tx);
                    return; // stopped right after initialize
                }
                // Hand the client to the App *before* launching, so it holds the
                // handle when the `initialized` event arrives (it sends breakpoints).
                let _ = output
                    .send(Message::DapStarted {
                        run,
                        client: client.clone(),
                        port,
                    })
                    .await;
                // That send is an await on a bounded channel — a real yield
                // point, and a Stop processed during it must still win. While
                // the session is `Launching` the App holds no client (Stop is
                // only a counter bump), so this is the ONLY place that can
                // stop the debuggee before it exists.
                if cancelled(&live) {
                    let _ = client.disconnect().await;
                    kill(&server_tx);
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
                        // Ask the adapter to terminate the debuggee, then reap
                        // it: killing the adapter alone can orphan the program
                        // it launched.
                        let _ = client.disconnect().await;
                        kill(&server_tx);
                        return; // stopped: no final Terminated for a dead run
                    }
                    if output
                        .send(Message::DapEvent { run, event: ev })
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
                    kill(&server_tx);
                    return;
                }
                // Adapter closed: make sure the session tears down.
                let _ = output
                    .send(Message::DapEvent {
                        run,
                        event: dap::DapEvent::Terminated,
                    })
                    .await;
            },
        );
        Task::run(stream, |m| m)
    }

    /// Tear down the work this window owns, just before the shell drops its
    /// `App` (see [`crate::shell`]'s `Shell::Closed`).
    ///
    /// The debug session is the part that needs a hook: the startup stream
    /// holding the adapter handle runs on the daemon's runtime, not on the
    /// window, so it survives the `App` and keeps the adapter (and the
    /// debuggee) alive. Stopping it is exactly what the Stop button does — the
    /// run identity moves on, the stream sees that at its next 150 ms
    /// checkpoint and disconnects + reaps what it spawned — so this defers to
    /// [`Self::on_debug_stop`]. Called unconditionally, even with no
    /// `debug.session`: a startup stream can outlive that field (a js-debug
    /// child session that fails to connect clears it while the parent adapter
    /// is still running), and the counter is the only handle on that stream.
    ///
    /// What this does NOT reap, so nobody reads a guarantee into it:
    /// * The window's clew-server. Its subscription vanishes with the `App`
    ///   and `kill_on_drop` reaps the child — but by SIGKILL, so the server's
    ///   own children (a proxied adapter, language servers) are not reaped
    ///   with it and are left to notice their closed stdio.
    /// * The Explain pass and the LSP call-graph refine. Both are iced
    ///   `Handle`s, which do not abort when dropped, so both keep running —
    ///   Explain keeps issuing (billable) LLM calls, and the refine keeps
    ///   clones of the language-server clients alive. Only
    ///   `drop_project_work` aborts them, and no window-close path calls it.
    pub(crate) fn on_window_closed(&mut self) -> Task<Message> {
        self.on_debug_stop()
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
                    move |answers| Message::DapBreakpointsAnswered {
                        run: bp_run,
                        answers,
                    },
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
                    move |(frames, scopes)| Message::DapStopInspected {
                        run,
                        stop,
                        frames,
                        scopes,
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
                let Some(port) = session.port else {
                    return Task::none();
                };
                let run = self.debug_run;
                let stream = iced::stream::channel(
                    64,
                    move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                        use iced::futures::SinkExt;
                        let (client, mut events) = match dap::DapClient::connect_tcp(port).await {
                            Ok(pair) => pair,
                            Err(e) => {
                                let _ = output.send(Message::DebugFailed { run, error: e }).await;
                                return;
                            }
                        };
                        if client.initialize().await.is_err() {
                            return;
                        }
                        let _ = output
                            .send(Message::DapChildStarted {
                                run,
                                client: client.clone(),
                            })
                            .await;
                        client.launch(config);
                        while let Some(ev) = events.recv().await {
                            if output
                                .send(Message::DapEvent { run, event: ev })
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
        // Stamped so a reply from a session the user has already stopped cannot
        // relabel the breakpoints of the next one, the same generation guard
        // `on_dap_event` and `eval_watches` use.
        let run = self.debug_run;
        Task::perform(
            async move {
                let answer = client.set_breakpoints(&p, &lines).await;
                vec![(p, answer)]
            },
            move |answers| Message::DapBreakpointsAnswered { run, answers },
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
                        .evaluate(&e, frame_id)
                        .await
                        .unwrap_or_else(|err| format!("⚠ {err}"));
                    out.push((e, v));
                }
                out
            },
            move |vals| Message::DebugWatchesEvaluated { run, stop, vals },
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

    /// Whether this project's `.clew/` state may touch the LOCAL filesystem.
    /// A remote project's root is a remote path: reading or creating a
    /// same-pathed `.clew/` on this machine would mix two hosts' data. Its
    /// state stays in memory until persistence migrates over the protocol.
    pub(crate) fn local_project_state(&self) -> bool {
        !self.connection.is_remote()
    }

    /// Whether an async result tagged `(root, epoch)` still belongs to the
    /// open project. Both halves matter: `epoch` alone is the authority (it
    /// distinguishes two projects that share an absolute path on different
    /// hosts, which a `root` comparison cannot), and `root` is kept as a
    /// cheap consistency check on the same instance.
    pub(crate) fn owns_result(&self, root: &Path, epoch: u64) -> bool {
        epoch == self.project_epoch && self.project.as_ref().is_some_and(|p| p.root == root)
    }

    /// Ask the server for the four `.clew/` session-state files of a REMOTE
    /// project (history, bookmarks, notes, reading target) — they live where
    /// the project lives; a same-pathed local file is another machine's
    /// data. The replies land as `StateContent` notifications.
    pub(crate) fn request_remote_state(&mut self) {
        let (Some(tx), Some(root)) = (
            self.server_tx.clone(),
            self.project
                .as_ref()
                .map(|p| p.root.to_string_lossy().into_owned()),
        ) else {
            return;
        };
        // Until each file's real content arrives, what this client holds for
        // it is an EMPTY baseline (`on_scan_done` starts every remote project
        // that way). A save in that window would push the baseline back and
        // wipe the remote file — these writes replace it wholesale, and an
        // empty list serializes to `None`, which DELETES it. So each rel is
        // marked outstanding here and only becomes WHOLESALE-writable when it
        // loads. It bounds `write_remote_state` only: an `EditState` names one
        // entry and carries no baseline, so it is safe to send straight away
        // (see `edit_remote_state`).
        //
        // The DIRTY set is deliberately kept: this also runs on a reconnect,
        // where it holds changes the user made while the link was down, and
        // dropping them here would lose exactly the edits this re-read exists
        // to rescue. A fresh project clears both in `on_scan_done`.
        self.remote_state_pending.clear();
        for rel in REMOTE_STATE_FILES {
            self.remote_state_pending.insert((*rel).to_string());
            let id = self
                .next_req_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send(clew_protocol::ClientMessage {
                id,
                request: clew_protocol::Request::ReadState {
                    root: root.clone(),
                    rel: (*rel).into(),
                },
            });
        }
    }

    /// Re-send the state file `rel` from what this client now holds. Used when
    /// the user changed it while its load was still outstanding: the load's
    /// arrival keeps the user's version, and this is what persists it.
    ///
    /// Whole-snapshot, and for the three MERGEABLE stores that is a last
    /// resort, not the normal path: an ordinary edit of those goes out as a
    /// `StateMerge` the server applies to the file (see [`Self::edit_remote_state`]).
    /// This is reached for them only when the edit could not be sent at all —
    /// no transport, or a transport that died before acknowledging — and it
    /// then does exactly what `EditState` exists to avoid: whatever another
    /// client wrote in the meantime is replaced by this window's copy. Keeping
    /// the user's offline edits is the reason it is still here; the residual is
    /// that they cost the other client's, once, on reconnect.
    pub(crate) fn flush_remote_state(&mut self, rel: &str) {
        let text = match rel {
            "history.json" => self
                .project
                .as_ref()
                .and_then(|p| history::to_text(&p.root, &self.history)),
            "bookmarks.json" => bookmarks::to_text(&self.bookmarks),
            "notes.json" => notes::to_text(&self.notes),
            "reading.toml" => reading::target_to_text(&self.reading_target),
            _ if rel == walkthrough::LIBRARY_REL => walkthrough::to_text(&self.walk.library),
            _ => return,
        };
        self.write_remote_state(rel, text);
    }

    /// Persist ONE tour change of a REMOTE project's library, by scope:
    /// `Some(tour)` upserts it, `None` deletes it.
    ///
    /// Scope, not index, and one tour, not the library: writing this window's
    /// whole library back erased every tour another client had generated
    /// since it loaded. The local arms of both mutation paths (generate at
    /// `on_walkthrough_ready`, delete at `on_walkthrough_delete`) already
    /// merge through `walkthrough::edit_library`; this is the same merge for
    /// the remote arm, performed at the server.
    ///
    /// The local arm below is still a WHOLE-library write, and is kept only as
    /// the "local project but no root" dispatch that nothing reaches today —
    /// not as a sanctioned way to save locally. A new local caller must go
    /// through `edit_library`, or it reintroduces the lost update.
    pub(crate) fn save_walkthrough_scope(
        &mut self,
        scope: &str,
        tour: Option<&walkthrough::Walkthrough>,
    ) {
        // Checked BEFORE the branch: with no project open there is nothing to
        // save anywhere, and the remote arm would otherwise write this
        // window's leftover tour into whatever project loads next.
        if self.project.is_none() {
            return;
        }
        if !self.local_project_state() {
            let merge = match tour {
                Some(tour) => walkthrough::merge_upsert(tour),
                None => Some(walkthrough::merge_remove(scope)),
            };
            match merge {
                Some(merge) => self.edit_remote_state(walkthrough::LIBRARY_REL, merge),
                // The tour could not be serialized, so there is nothing to
                // send. Saying so beats a silent no-op the user reads as saved.
                None => self.status = "Could not save walkthrough: it is not serializable".into(),
            }
            return;
        }
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = walkthrough::save_library(&root, &self.walk.library)
        {
            self.status = format!("Could not save walkthrough: {e}");
        }
    }

    /// Apply ONE entry-level change to a REMOTE project's `.clew/<rel>` at the
    /// server, and adopt the merged file it replies with.
    ///
    /// This is the remote half of the same rule the local stores follow: apply
    /// the change to the CONTENT THAT IS AUTHORITATIVE RIGHT NOW, never to a
    /// window's copy of it. Locally that is a read-modify-write under
    /// `bookmarks::edit` / `notes::edit` / `walkthrough::edit_library`; here no
    /// client can hold that lock — two windows on one remote project each open
    /// their own SSH session and their own remote clew-server — so the change
    /// travels as data and the server performs the read-modify-write. The
    /// merged file comes back as `StateEdited` and replaces this window's copy,
    /// exactly as the local callers adopt the merged list.
    ///
    /// Deliberately NOT gated on `remote_state_pending`, which
    /// [`Self::write_remote_state`] must be: that gate exists because a
    /// wholesale write from a client that has not loaded the file yet pushes
    /// its empty baseline over the remote's content. A merge carries no
    /// baseline — it names one entry and what to do with it — so it is safe
    /// the moment the user makes it, even before the initial read lands.
    pub(crate) fn edit_remote_state(&mut self, rel: &str, merge: clew_protocol::StateMerge) {
        let Some(root) = self
            .project
            .as_ref()
            .map(|p| p.root.to_string_lossy().into_owned())
        else {
            return;
        };
        // Dirty FIRST, and cleared only by the server's `StateEdited`: a
        // successful `send` proves nothing (a transport that has died without
        // being detected accepts frames into a pipe that goes nowhere), and
        // what is dirty is re-sent on reconnect.
        self.remote_state_dirty.insert(rel.to_string());
        let Some(tx) = self.server_tx.clone() else {
            // No transport. The caller has already applied the change to this
            // window's copy, so the reconnect's `flush_remote_state` is what
            // persists it — as a whole snapshot, with the loss that implies.
            return;
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tx
            .send(clew_protocol::ClientMessage {
                id,
                request: clew_protocol::Request::EditState {
                    root,
                    rel: rel.into(),
                    merge,
                },
            })
            .is_err()
        {
            // The writer task is gone. Stay dirty; the reconnect flushes.
            return;
        }
        // Supersede any earlier write of the same rel: its acknowledgement
        // must not clear a mark this newer one owns.
        //
        // `remote_state_rescue` is deliberately NOT superseded here. That map
        // does not track ownership of the dirty mark, it tracks which request
        // carries a change the server never received — a fact this newer edit
        // does not take over, since its merge is computed WITHOUT that change.
        // Dropping the flush's id here too left both marks stranded forever
        // (see the field's own doc).
        self.remote_state_inflight
            .retain(|_, pending| pending != rel);
        self.remote_state_inflight.insert(id, rel.to_string());
    }

    /// Whether a change to `rel` is on its way to the server and has not been
    /// acknowledged. A `StateContent` that arrives while one is outstanding
    /// describes the file BEFORE that change: adopting it would revert what the
    /// user just did, and re-flushing this window's snapshot over it would undo
    /// the very merge the outstanding edit is there to get.
    pub(crate) fn remote_state_edit_inflight(&self, rel: &str) -> bool {
        self.remote_state_inflight.values().any(|r| r == rel)
    }

    /// Replace one `.clew/<rel>` of a REMOTE project wholesale (`None`
    /// deletes). Fire-and-forget: a failure comes back as an Error event and
    /// lands in the status bar.
    ///
    /// Correct only for a store this client alone owns the whole content of —
    /// `history.json` (deliberately last-writer-wins, see [`Self::save_history`])
    /// and `reading.toml` (a single scalar). For the stores two clients can
    /// both add entries to, use [`Self::edit_remote_state`]: a snapshot written
    /// from a copy loaded at project open deletes everything the other client
    /// has written since.
    pub(crate) fn write_remote_state(&mut self, rel: &str, text: Option<String>) {
        let Some(root) = self
            .project
            .as_ref()
            .map(|p| p.root.to_string_lossy().into_owned())
        else {
            return;
        };
        // Dirty FIRST, and cleared only by the server's `StateWritten`. A
        // successful `send` proves nothing: it queues a frame, and a transport
        // that has died without being detected — a laptop changing networks,
        // the ordinary case — accepts frames into a pipe that goes nowhere.
        // Clearing the mark here lost every change made in that window, and
        // the reconnect's re-read then replaced them with the stale remote
        // copy (see `remote_state_inflight`).
        self.remote_state_dirty.insert(rel.to_string());
        // Two reasons to hold a change back rather than send it, and both end
        // the same way: it stays dirty and `flush_remote_state` sends it once
        // the (re-)read lands.
        //
        // * the file's real content has not arrived yet, so writing now would
        //   push this client's empty baseline over it;
        // * there is no transport at all.
        let tx = match &self.server_tx {
            Some(tx) if !self.remote_state_pending.contains(rel) => tx.clone(),
            _ => return,
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if tx
            .send(clew_protocol::ClientMessage {
                id,
                request: clew_protocol::Request::WriteState {
                    root,
                    rel: rel.into(),
                    text,
                },
            })
            .is_err()
        {
            // The writer task is gone. Stay dirty; the reconnect flushes.
            return;
        }
        // Supersede any earlier write of the same rel: its acknowledgement
        // must not clear a mark this newer one owns.
        self.remote_state_inflight
            .retain(|_, pending| pending != rel);
        self.remote_state_inflight.insert(id, rel.to_string());
        // These bytes are this window's whole copy, so if a change the server
        // never received is in that copy, this request is what carries it back.
        // Recorded separately from the id above because a later edit takes the
        // dirty mark's ownership away from this id while leaving that fact
        // true — and then only this record can retire the unsent mark.
        if self.remote_state_unsent.contains(rel) {
            self.remote_state_rescue.insert(id, rel.to_string());
        }
    }

    /// Persist the navigation tree to the project's `.clew/` — on the local
    /// disk, or over the protocol for a remote project. Errors are ignored
    /// (a read-only project just keeps its history for the session).
    ///
    /// Whole-tree write on purpose: with the project open in two windows the
    /// one that navigated last owns the stored trail (see `history::save` for
    /// why merging two readers' trees would be worse than that).
    pub(crate) fn save_history(&mut self) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        if self.local_project_state() {
            let _ = history::save(&root, &self.history);
        } else {
            let text = history::to_text(&root, &self.history);
            self.write_remote_state("history.json", text);
        }
    }

    /// Apply one change to the reading notes and persist it.
    ///
    /// The change is handed down rather than applied to `self.notes` first,
    /// because locally it is replayed on the list read under the store lock:
    /// `self.notes` is this window's copy from project open, so writing it
    /// wholesale erased every note a second window on the same project had
    /// written since — invisibly, since each window kept rendering its own copy
    /// until the next launch (see `notes::edit`). The merged list is adopted so
    /// this window stops disagreeing with disk.
    ///
    /// Remotely the same change goes out as `merge`, which the SERVER replays
    /// on the file — the only place both clients' writes are visible. The
    /// local change still runs on `self.notes` there, so the UI does not wait
    /// for the round trip; the merged file that comes back replaces it.
    pub(crate) fn edit_notes(
        &mut self,
        merge: clew_protocol::StateMerge,
        change: impl FnOnce(&mut Vec<notes::Note>),
    ) {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return;
        };
        if !self.local_project_state() {
            change(&mut self.notes);
            self.edit_remote_state(notes::REL, merge);
            return;
        }
        // The merged list is adopted whether or not the write landed. The
        // caller has already taken the user's draft (`NoteEditSave` empties
        // `reading_note_edit` before getting here), so on an unwritable
        // `.clew/` — a read-only checkout, a full disk — dropping it deleted
        // prose that existed nowhere else, the moment the user pressed save.
        // Kept in memory it is still readable and re-savable this session;
        // only the persistence failed, and the status line says exactly that.
        let (merged, saved) = notes::edit(&root, change);
        self.notes = merged;
        if let Err(e) = saved {
            self.status =
                format!("Cannot write .clew/notes.json: {e} — kept for this session, not saved");
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
        // Bumped with every reset, not only the explicit exit: a load still in
        // flight is guarded ONLY by this generation, so without it a late
        // TimeTravelReady re-installs a session for the file we just left, and
        // its first Goto then resolves against whatever pane is open now.
        self.time_gen += 1;
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
            // Cancel any load still in flight for this pane (A → B → A: B's
            // reply would otherwise land and replace the A the user is
            // looking at). The token is what makes it a no-op on arrival.
            self.pane_pending[pane] = None;
            let Some(v) = self.active_viewer_mut() else {
                return Task::none();
            };
            v.target_line = line;
            if let Some(l) = line {
                let l0 = l.saturating_sub(1);
                v.reveal(l0); // expand any fold hiding the jump target
                v.caret = Some((l0, 0));
            }
            // Notebook cells have variable height, so their goto scrolls to an
            // estimated cell offset (the target ring points precisely).
            let y = match (&v.notebook, line) {
                (Some(doc), Some(l)) => {
                    Self::estimate_notebook_offset(doc, &v.nb_expanded, l, line_height)
                }
                _ => v.scroll_offset_for(line, line_height),
            };
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
                // This is now the one load the pane is waiting for; any
                // earlier in-flight load for it is superseded.
                self.pane_pending[pane] = Some(id);
                self.status = format!("Loading {rel}…");
                return Task::none();
            }
        }
        // Remote project with the transport down: fail closed. The path names
        // a file on the remote host; a local file at the same absolute path
        // is a different machine's data, so reading it here would silently
        // show (and index) the wrong project.
        if self.connection.is_remote() {
            self.pane_pending[pane] = None;
            self.status = format!("Disconnected from the remote host — cannot open {rel}");
            return Task::none();
        }
        // Fallback: server not up — read + highlight locally. The token comes
        // from the same id space as server reads, so the pane guard is uniform.
        let req = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pane_pending[pane] = Some(req);
        self.status = format!("Loading {rel}…");
        Task::perform(
            load_file(pane, abs, line),
            move |(pane, abs, target, result)| Message::FileLoaded {
                req,
                pane,
                abs,
                target,
                result,
            },
        )
    }

    pub(crate) fn on_file_loaded(
        &mut self,
        req: u64,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    ) -> Task<Message> {
        // Only the load the pane is still waiting for may land: a slower
        // earlier open must not overwrite a faster later one, and a load
        // issued before a project switch must not resurrect into it.
        if self.pane_pending.get(pane).copied().flatten() != Some(req) {
            return Task::none();
        }
        self.pane_pending[pane] = None;
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
        let hl_target = target.clone();
        // Stamped from the bytes being highlighted, so a result that arrives
        // after a newer pass can be recognized as stale and dropped.
        let hl_hash = incremental::content_hash(source.as_bytes());
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
                src_hash: hl_hash,
                lines,
                symbols,
                docs,
                inactive,
                target: hl_target.clone(),
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
        // Time Travel history navigation uses COMMAND chords — Cmd+←/Cmd+h step
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
    /// The cached explanation to show on hover: the full summary (the tooltip
    /// wraps and scrolls, so no first-sentence truncation here).
    pub(crate) fn hover_summary(&self, pane: usize, line: usize, col: usize) -> Option<String> {
        let v = self.panes.get(pane)?.as_ref()?;
        let usable = |s: &str| (!explain::is_error_summary(s)).then(|| s.trim().to_string());
        if let Some(word) = analyze::word_at(&v.lines, line, col) {
            // Same-file definition wins (unambiguous).
            // Same-file definition wins; a word can't say which same-name
            // overload it means, so take the first (ordinal 0).
            if let Some(c) = self.explain.cache.get(&explain::Node::Function {
                file: v.abs.clone(),
                name: word.clone(),
                ordinal: 0,
            }) {
                return usable(&c.summary);
            }
            // Otherwise, only if exactly one explained function has this name.
            let mut hit: Option<&str> = None;
            let mut ambiguous = false;
            for (node, c) in &self.explain.cache {
                if let explain::Node::Function { name, .. } = node
                    && name == &word
                {
                    if hit.is_some() {
                        ambiguous = true;
                        break;
                    }
                    hit = Some(&c.summary);
                }
            }
            if !ambiguous && let Some(s) = hit {
                return usable(s);
            }
        }
        // Anywhere on a function's signature line reads as hovering that
        // function — this replaces the old end-of-line inline summary chip.
        let sig = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .find(|s| s.line == line + 1)?;
        let c = self.explain.cache.get(&explain::Node::Function {
            file: v.abs.clone(),
            name: sig.name.clone(),
            ordinal: outline::fn_ordinal(&v.symbols, sig),
        })?;
        usable(&c.summary)
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

    /// The document the active pane is showing right now, as the identity a
    /// find-match list is stamped with. `None` when no pane holds a file.
    pub(crate) fn active_find_doc(&self) -> Option<find::DocId> {
        self.active_viewer()
            .map(|v| (v.abs.clone(), Arc::as_ptr(&v.source) as usize))
    }

    /// Recompute the find matches over the active pane's current document.
    pub(crate) fn recompute_find(&mut self) {
        let Some(doc) = self.active_find_doc() else {
            // Nothing on screen to match against, so nothing may be painted:
            // keeping the previous file's triples here is what let them be
            // drawn over the next document that arrives.
            self.find.matches.clear();
            self.find.current = 0;
            self.find.doc = None;
            return;
        };
        let Some(lines) = self.active_viewer().map(|v| v.lines.clone()) else {
            return;
        };
        self.find.recompute(doc, &lines);
    }

    /// Keep the find matches describing the document actually on screen.
    ///
    /// `find.matches` are raw (line, col0, col1) triples in ONE document's
    /// coordinates, and every consumer — the painted highlight rectangles, the
    /// `n/m` counter, the caret jump — uses them without re-checking which
    /// file that was. Opening another file into the pane, reloading the same
    /// file after an on-disk change, and clicking into the other half of a
    /// split all leave the bar open with the previous document's triples, so
    /// the highlights land on unrelated substrings and Enter parks the caret
    /// where the query does not occur. Recompute (rather than clear) so the
    /// user's query survives the move and `current` re-anchors near the same
    /// line. Called once per update, so a new way to swap a pane's document
    /// cannot forget it — the identity stamp makes the check a no-op when
    /// nothing changed.
    pub(crate) fn sync_find_matches(&mut self) {
        if !self.find.open {
            return;
        }
        if self.find.doc == self.active_find_doc() {
            return;
        }
        self.recompute_find();
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
        if let Some(doc) = v.notebook.clone() {
            // Notebook: the view can't paint per-match highlights, so point at
            // the owning cell instead — estimated scroll plus the target ring.
            v.target_line = Some(line + 1);
            v.scroll_y =
                Self::estimate_notebook_offset(&doc, &v.nb_expanded, line + 1, line_height);
        } else {
            // Center-ish the match line (display rows account for folds).
            let top = v.row_of(line) as f32 * line_height;
            if top < v.scroll_y || top + line_height > v.scroll_y + v.viewport_h {
                v.scroll_y = (top - v.viewport_h / 3.0).max(0.0);
            }
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
            let occ = v.occurrences(&word, 500);
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
        if let Some((ml, mc)) = v.matching_bracket(line, col) {
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
            port: None,
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
