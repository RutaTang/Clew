//! Language servers: provisioning (registry, store, download consent,
//! repository commands staged and approved), starting local or proxied
//! servers, keeping documents open and in sync, inlay hints, the per-update
//! state snapshots, and the Language Servers panel.
//!
//! Its messages, [`LspMsg`], arrive through `App::update_lsp`.

use crate::app::prelude::*;
use crate::*;

/// How much of an `init_options` blob the approval modal shows. `lsp.toml` is
/// repository-controlled and may be up to a megabyte, and the modal is drawn
/// on the update thread — a config that pads its options must not be able to
/// wedge the window that is asking about it. What is elided is still inside
/// the fingerprint being approved, so the notice below says so plainly rather
/// than letting the user believe they saw all of it.
const MAX_SHOWN_INIT_OPTIONS: usize = 4000;

/// How many of a server's newest log lines the Language Servers panel shows.
pub(crate) const SERVER_LOG_LINES: usize = 200;

/// The longest a restarted language server's spawn waits behind the kill of
/// the instance it replaces (`App::start_lsp_with`). That kill is sent at
/// once but for a full request queue, and a bridge that cannot send it must
/// not hold the new server up for good.
const REPLACED_KILL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The `init_options` an approval modal shows, pretty-printed (the raw JSON of
/// a nested table is unreadable, and this is the text the user's decision
/// rests on) and bounded.
pub(crate) fn pretty_init_options(options: Option<&serde_json::Value>) -> Option<String> {
    let options = options?;
    let mut text = serde_json::to_string_pretty(options).unwrap_or_else(|_| format!("{options:?}"));
    if text.chars().count() > MAX_SHOWN_INIT_OPTIONS {
        let cut = text
            .char_indices()
            .nth(MAX_SHOWN_INIT_OPTIONS)
            .map(|(i, _)| i)
            .unwrap_or(text.len());
        text.truncate(cut);
        text.push_str("\n… truncated for display — the approval covers the whole file's options");
    }
    Some(text)
}

/// A language-server install that runs on THIS machine, once the user
/// consented to it (see `App::on_lsp_consent_allowed`).
enum LocalInstall {
    Download(lsp::registry::Download),
    Toolchain(lsp::registry::Install),
}

impl App {
    /// Read every READY language server's state once per update — one lock
    /// each — for the view that follows: the status bar, the diagnostic
    /// underlines, the hover's diagnostic and the server panel all take their
    /// data from these snapshots instead of each locking the server's state
    /// and cloning what it needs. A snapshot shares the diagnostics with the
    /// live state (see `lsp::client::Snapshot`), so this copies none of them.
    /// A poisoned state is kept as the `Err` it is, for the view to report.
    pub(crate) fn sync_lsp_snapshots(&mut self) {
        let lsp = &self.proj.link.lsp;
        self.proj
            .link
            .lsp_snapshots
            .retain(|lang, _| matches!(lsp.get(lang), Some(LspSlot::Ready(_))));
        for (lang, slot) in lsp.iter() {
            if let LspSlot::Ready(client) = slot {
                let snapshot = client.snapshot();
                match self.proj.link.lsp_snapshots.get_mut(lang) {
                    Some(held) => *held = snapshot,
                    None => {
                        self.proj.link.lsp_snapshots.insert(lang.clone(), snapshot);
                    }
                }
            }
        }
        self.sync_lsp_log();
    }

    /// Keep the Language Servers panel's log current — while the panel is
    /// open, for the active file's server — copying its newest lines only
    /// when the log moved since the last copy (the snapshot's
    /// `log_version`). The panel used to copy the whole log on every repaint.
    pub(crate) fn sync_lsp_log(&mut self) {
        let lang = self
            .active_viewer()
            .and_then(|v| v.lang_key)
            .filter(|_| self.server_panel);
        let client = match lang.and_then(|l| self.proj.link.lsp.get(l)) {
            Some(LspSlot::Ready(client)) => client,
            _ => {
                self.proj.link.lsp_log = None;
                return;
            }
        };
        let lang = lang.unwrap_or_default();
        // A poisoned state has no version: read again (the read is the
        // error, cheaply) rather than keep a stale copy.
        let version = match self.proj.link.lsp_snapshots.get(lang) {
            Some(Ok(snapshot)) => Some(snapshot.log_version),
            _ => None,
        };
        if let (Some(held), Some(version)) = (&self.proj.link.lsp_log, version)
            && held.language == lang
            && held.version == version
        {
            return;
        }
        self.proj.link.lsp_log = Some(LspLogTail {
            language: lang.to_string(),
            version: version.unwrap_or(u64::MAX),
            lines: client.log_tail(SERVER_LOG_LINES),
        });
    }

    pub(crate) fn lsp_needs_refresh(&self) -> bool {
        self.server_panel
            || self.proj.link.lsp.iter().any(|(lang, s)| match s {
                LspSlot::Starting => true,
                // Read from this update's snapshot (`sync_lsp_snapshots`), not
                // by locking every server's state three more times.
                LspSlot::Ready(_) => match self.proj.link.lsp_snapshots.get(lang) {
                    Some(Ok(snap)) => {
                        snap.progress.is_some()
                            || self.proj.link.seen_diag_version.get(lang).copied()
                                != Some(snap.diag_version)
                            || self.proj.link.seen_inlay_epoch.get(lang).copied()
                                != Some(snap.inlay_epoch)
                    }
                    // Not read yet: one tick takes the first reading.
                    None => true,
                    // A poisoned state never changes again; polling it is noise.
                    Some(Err(_)) => false,
                },
                _ => false,
            })
    }

    /// Ensure a language server is provisioned/started for `language`, and open
    /// any already-loaded documents once it is ready. Idempotent.
    pub(crate) fn ensure_lsp(&mut self, language: &str) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        match self.proj.link.lsp.get(language) {
            Some(LspSlot::Ready(client)) if client.alive() => {
                let client = client.clone();
                return self.open_docs_for_language(language, &client);
            }
            // A server that died after startup (crash, OOM-kill, or a locally
            // spawned one, which gets no `ProcessExited` at all) leaves a client
            // whose every request fails. Discard it and fall through to a fresh
            // start, or the language stays dead for the rest of the session.
            Some(LspSlot::Ready(_)) => self.reset_lsp(language),
            // Starting / failed / unsupported / awaiting consent: nothing to do.
            Some(_) => return Task::none(),
            None => {}
        }

        // Remote project: everything about the server (its lsp.toml, its
        // binaries, its command approvals) lives on the remote host. Ask the
        // server what it would run; the LspResolved reply either starts it
        // straight away or raises the approval modal with the real remote
        // command line. Local provisioning is skipped entirely — downloading
        // a binary here for a server that runs over there was pure waste.
        if self.connection.is_remote() {
            if self.server.is_up() {
                let request = clew_protocol::Request::LspResolve {
                    language: language.to_string(),
                };
                if self.send_to_server(request).is_some() {
                    self.proj
                        .link
                        .lsp
                        .insert(language.to_string(), LspSlot::AwaitingConsent);
                    return Task::none();
                }
            }
            return Task::none(); // no transport: retry on the next ensure
        }

        // The project's `lsp.toml` is read with the rest of its state, off the
        // UI thread. Until it has landed, the config here is the default one,
        // and starting from it could run a different server than the project
        // configures: wait, and start once it is known
        // (`on_project_state_loaded`).
        if self.proj.inflight.state_loading {
            if !self
                .proj
                .inflight
                .deferred_lsp
                .iter()
                .any(|l| l == language)
            {
                self.proj.inflight.deferred_lsp.push(language.to_string());
            }
            return Task::none();
        }
        let Some(server) = self.proj.lsp_config.resolve(language) else {
            self.proj.link.lsp.insert(
                language.to_string(),
                LspSlot::Unsupported("no server for this language".into()),
            );
            return Task::none();
        };
        let (provision, dest_dir) = match lsp::store::locate(&server) {
            // A store-installed binary: its install consent already covered it.
            // (The `init_options` half of `lsp.toml` is gated in
            // `start_lsp_with`, the one place every start funnels through.)
            lsp::store::Located::Ready(exe) => return self.start_lsp_with(language, exe),
            // A `command` in the project's own lsp.toml names an arbitrary
            // executable, and that file ships with the repository — so it must
            // be shown and approved before it runs, and what runs is clew's
            // private copy of the approved bytes (hashing a path and then
            // spawning that path is a race the repository wins by swapping
            // it). Hashing it reads up to a gigabyte, so it happens on the
            // blocking pool; the result lands as `LspMsg::Staged`.
            lsp::store::Located::RepoCommand(command) => {
                return self.stage_lsp_command_off_thread(language, command, server);
            }
            lsp::store::Located::NeedsDownload { download, dest_dir } => {
                (LspProvision::Download(download), dest_dir)
            }
            lsp::store::Located::NeedsInstall {
                install, dest_dir, ..
            } => (LspProvision::Install(install), dest_dir),
            lsp::store::Located::Unsupported(msg) => {
                self.proj
                    .link
                    .lsp
                    .insert(language.to_string(), LspSlot::Unsupported(msg));
                return Task::none();
            }
        };
        self.proj
            .link
            .lsp
            .insert(language.to_string(), LspSlot::AwaitingConsent);
        self.proj.pending_lsp_consent.offer(LspConsent {
            language: language.to_string(),
            server_name: server.server_name,
            version: server.version,
            provision,
            dest_dir,
        });
        Task::none()
    }

    /// Hash the repository-named `command` (with its args and init options —
    /// the same repo-shipped input, and several servers run programs named in
    /// them) on the blocking pool and, when that fingerprint is already
    /// approved, stage clew's private copy of the bytes. The slot reads
    /// "starting" meanwhile, so a second `ensure_lsp` does not stage twice; a
    /// restart or project switch in between retires the result through the
    /// spawn generation and the project epoch.
    fn stage_lsp_command_off_thread(
        &mut self,
        language: &str,
        command: PathBuf,
        server: lsp::config::EffectiveServer,
    ) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // The approval on record for this project and language — compared in
        // the task, so the trust store itself never leaves the UI thread.
        let approved: Option<String> = self
            .trust
            .lsp_approvals_for(None, &root)
            .into_iter()
            .find(|(lang, _)| lang == language)
            .map(|(_, fingerprint)| fingerprint);
        let generation = self.next_lsp_gen(language);
        self.proj
            .link
            .lsp
            .insert(language.to_string(), LspSlot::Starting);
        // Transport-bound: a disconnect drops every language-server slot, so a
        // staging that outlives one has no slot left to fill.
        let stamp = self.transport_stamp();
        let lang = language.to_string();
        let task_root = root;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let probe = clew_core::trust::probe_lsp_command(
                        &task_root,
                        &command,
                        &server.args,
                        &server.server_name,
                        &server.version,
                        server.init_options.as_ref(),
                    )?;
                    let fingerprint = probe.fingerprint().to_string();
                    let source = probe.source().to_path_buf();
                    // Only an approved probe is materialized.
                    let exec_path = if approved.as_deref() == Some(fingerprint.as_str()) {
                        Some(probe.into_exec()?)
                    } else {
                        None
                    };
                    Ok(LspStagedCommand {
                        fingerprint,
                        source,
                        exec_path,
                        server,
                    })
                })
                .await
                .unwrap_or_else(|e| Err(format!("staging the command failed: {e}")))
            },
            move |result| {
                Message::Lsp(LspMsg::Staged {
                    stamp: stamp.clone(),
                    language: lang.clone(),
                    generation,
                    result,
                })
            },
        )
    }

    /// The off-thread staging of a repository command finished. Start the
    /// approved copy, or raise the approval modal with the command line the
    /// user is being asked about — unless the staging is no longer wanted (a
    /// restart, a project switch) or `lsp.toml` changed while it ran, in which
    /// case the language is resolved afresh.
    pub(crate) fn on_lsp_staged(
        &mut self,
        language: String,
        generation: u64,
        root: PathBuf,
        result: Result<LspStagedCommand, String>,
    ) -> Task<Message> {
        // (The project and the transport were checked in `dispatch`.)
        if self.lsp_gen.get(&language) != Some(&generation) {
            return Task::none();
        }
        let staged = match result {
            Ok(staged) => staged,
            Err(e) => {
                // Unreadable command: can't be approved, can't run.
                self.proj
                    .link
                    .lsp
                    .insert(language, LspSlot::Failed(format!("lsp.toml command: {e}")));
                return Task::none();
            }
        };
        if self.proj.lsp_config.resolve(&language).as_ref() != Some(&staged.server) {
            self.proj.link.lsp.remove(&language);
            return self.ensure_lsp(&language);
        }
        let Some(exec) = staged.exec_path else {
            self.proj
                .link
                .lsp
                .insert(language.clone(), LspSlot::AwaitingConsent);
            // The modal shows (and the approval records) the REPOSITORY's path
            // — that is what the user is being asked about. Approving
            // re-enters `ensure_lsp`, which stages afresh, so nothing here can
            // be spawned later.
            self.proj.pending_lsp_command = Some(PendingLspCommand {
                root,
                host: None,
                language,
                command: Some(staged.source),
                args: staged.server.args.clone(),
                server_name: staged.server.server_name.clone(),
                version: staged.server.version.clone(),
                fingerprint: staged.fingerprint,
                // Shown with the command line: the options are inside this
                // fingerprint, so the user is approving them too and must be
                // able to read them.
                init_options: pretty_init_options(staged.server.init_options.as_ref()),
            });
            return Task::none();
        };
        // Remembered so the start below need not hash the command a second
        // time to vouch for the options it carries (`approved_init_options`).
        self.proj
            .inflight
            .lsp_staged
            .insert(language.clone(), (staged.server, exec.clone()));
        self.start_lsp_with(&language, exec)
    }

    /// Remember the init options a remote `LspResolved` carried for
    /// `language` — `start_lsp_with` hands them to the client-side LSP
    /// handshake. A reply without options drops any stale entry.
    pub(crate) fn stash_remote_init(&mut self, language: &str, init: Option<serde_json::Value>) {
        match init {
            Some(v) => {
                self.proj.remote_lsp_init.insert(language.to_string(), v);
            }
            None => {
                self.proj.remote_lsp_init.remove(language);
            }
        }
    }

    /// The repository's own `init_options` for `language`, but only once the
    /// user has approved them: `Ok(..)` is what may go into `initialize`,
    /// `Err(())` means nothing may start (the approval modal is up, or the
    /// slot carries the reason).
    ///
    /// Options need consent in their own right, not as a footnote to a
    /// `command`. `.clew/lsp.toml` ships with the repository, its options
    /// reach the server verbatim, and servers read them as a place to name
    /// programs they then run: rust-analyzer's
    /// `cargo.buildScripts.overrideCommand` (run on workspace load, so merely
    /// opening a file is enough), `procMacro.server`,
    /// typescript-language-server's `tsserver.path`, pyright's
    /// `python.pythonPath`. The last one is the sharpest: `langenv` picks the
    /// interpreter through `runs_foreign_bytes` precisely so repository bytes
    /// are never nominated for execution, and `deep_merge` lets an explicit
    /// value WIN — so ungated options do not merely bypass that gate, they
    /// overrule it.
    ///
    /// One invariant, two shapes of approval: the options in hand must be
    /// covered by a fingerprint on record. A config with a `command` folds
    /// them into that command's fingerprint; one without has no bytes to hash
    /// and is fingerprinted on its own. The command case is re-derived here
    /// rather than assumed from `ensure_lsp`, because a rescan can swap
    /// `lsp_config` between that staging and this start.
    fn approved_init_options(
        &mut self,
        language: &str,
        server: &lsp::config::EffectiveServer,
    ) -> Result<Option<serde_json::Value>, ()> {
        let Some(options) = server.init_options.clone() else {
            return Ok(None); // nothing repo-controlled to approve
        };
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Err(());
        };
        // A command config that `on_lsp_staged` just approved and staged, for
        // exactly this configuration: its fingerprint covered these options,
        // and re-hashing the command (up to a gigabyte) here — on the UI
        // thread — would only re-derive the same answer. What runs is the
        // staged copy, so the file changing since cannot matter.
        if server.command.is_some()
            && self
                .proj
                .inflight
                .lsp_staged
                .get(language)
                .is_some_and(|(staged, _)| staged == server)
        {
            return Ok(Some(options));
        }
        let fingerprint = match &server.command {
            Some(cmd) => clew_core::trust::lsp_fingerprint(
                &root,
                cmd,
                &server.args,
                &server.server_name,
                &server.version,
                Some(&options),
            ),
            None => clew_core::trust::lsp_options_fingerprint(
                &server.args,
                &server.server_name,
                &server.version,
                &options,
            ),
        };
        let fingerprint = match fingerprint {
            Ok(fingerprint) => fingerprint,
            Err(e) => {
                // Unfingerprintable: it can be neither approved nor sent.
                self.proj.link.lsp.insert(
                    language.to_string(),
                    LspSlot::Failed(format!("lsp.toml: {e}")),
                );
                return Err(());
            }
        };
        if self
            .trust
            .is_lsp_approved(None, &root, language, &fingerprint)
        {
            return Ok(Some(options));
        }
        self.proj
            .link
            .lsp
            .insert(language.to_string(), LspSlot::AwaitingConsent);
        // No repo-named command in the options-only case: what runs is clew's
        // own store binary, which the install consent already covered.
        // Claiming a command line here would ask the user about the wrong
        // thing. When there IS one, show it the way the spawn resolves it,
        // not the raw relative string.
        let command = server
            .command
            .as_ref()
            .map(|c| clew_core::trust::resolve_command(&root, c));
        // Approving re-enters `ensure_lsp`, which resolves and re-fingerprints
        // from scratch — so an lsp.toml edited while the dialog sat open is
        // asked about again instead of riding on this answer.
        self.proj.pending_lsp_command = Some(PendingLspCommand {
            root,
            host: None,
            language: language.to_string(),
            command,
            args: server.args.clone(),
            server_name: server.server_name.clone(),
            version: server.version.clone(),
            fingerprint,
            init_options: pretty_init_options(Some(&options)),
        });
        Err(())
    }

    /// Launch the server executable and run the handshake in the background.
    pub(crate) fn start_lsp_with(&mut self, language: &str, exe: PathBuf) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // Local: args and init options come from the locally loaded lsp.toml.
        // Remote: the clew-server resolves its own config for the spawn, so
        // the local config (often empty — the root is a remote path) must not
        // gate the start; the client needs only the init options, which came
        // with `LspResolved`, because the LSP handshake itself still runs
        // client-side over the proxied stdio. The remote's options are gated
        // where its config is read (clew-server's `resolve_lsp`): only approved
        // ones ever arrive in `Ready::init_options`, and only those are stashed
        // (`stash_remote_init`), so there is nothing left to check here — and
        // nothing here COULD check them, since the fingerprint covers the
        // remote host's server/version/args. Withheld options do come back, in
        // `Ready::withheld`, but that copy only feeds the approval modal and is
        // never stashed. langenv is skipped for a remote: it probes the
        // filesystem, and this is the wrong host.
        let (args, init) = if self.connection.is_remote() {
            (Vec::new(), self.proj.remote_lsp_init.get(language).cloned())
        } else {
            let Some(server) = self.proj.lsp_config.resolve(language) else {
                return Task::none();
            };
            // The repository's own options reach `initialize` only once the
            // user has approved them. This is the choke point for that: every
            // local start arrives here — `ensure_lsp`'s ready arm, a finished
            // download/install (`LspMsg::DownloadResult`), a restart — and a gate
            // in any one of them would leave the others open.
            let explicit = match self.approved_init_options(language, &server) {
                Ok(explicit) => explicit,
                // The modal is up (or the slot failed); nothing may start.
                Err(()) => return Task::none(),
            };
            // Merge the auto-detected language environment (e.g. a project
            // venv for Python) under any explicit lsp.toml init_options
            // (explicit wins).
            let init = langenv::merge(language, &server.server_name, &root, explicit);
            (server.args.clone(), init)
        };
        self.proj
            .link
            .lsp
            .insert(language.to_string(), LspSlot::Starting);
        let lang = language.to_string();

        // Preferred: spawn the language server on clew-server and proxy its
        // stdio, so it runs where the code lives (local today, remote later).
        if let Some(tx) = self.server.tx().cloned() {
            // Stop a previous instance for this language (a restart) first:
            // letting go of its feed has its bridge send it its one kill
            // (`tasks::proxy_streams`), and the new one is spawned only once
            // that is sent — behind it, down the same request queue — so the
            // server is told to stop the old server before it starts the new.
            // Spawned at once, the new one went first, and the two ran side by
            // side until the kill caught up. The new one's own bridge sends
            // its spawn (`tasks::proxy_streams_spawning`), which puts that
            // spawn ahead of the kill the bridge sends it in turn.
            let replaced = self
                .server
                .lsp_procs
                .remove(&lang)
                .and_then(|old| self.server.proc_feeds.remove(&old))
                .map(|feed| {
                    let killed = feed.killed();
                    async move {
                        let _ = tokio::time::timeout(REPLACED_KILL_WAIT, killed).await;
                    }
                });
            let proc = self.next_proc_id;
            self.next_proc_id += 1;
            self.server.lsp_procs.insert(lang.clone(), proc);

            // Remote: the server resolves and runs its OWN language server, so we
            // never ship a binary path. Local: send the client-resolved binary.
            let spawn = if self.connection.is_remote() {
                clew_protocol::Request::SpawnLsp {
                    proc,
                    language: lang.clone(),
                }
            } else {
                clew_protocol::Request::SpawnProcess {
                    proc,
                    cmd: exe.to_string_lossy().into_owned(),
                    args: args.clone(),
                    cwd: Some(root.to_string_lossy().into_owned()),
                }
            };
            let (client_stdin, client_stdout, feed) =
                proxy_streams_spawning(&tx, &self.next_req_id, proc, spawn, replaced);
            self.server.proc_feeds.insert(proc, feed);

            let generation = self.next_lsp_gen(&lang);
            let lang_done = lang.clone();
            let stamp = self.transport_stamp();
            return Task::perform(
                async move {
                    lsp::client::LspClient::connect(client_stdin, client_stdout, &root, init).await
                },
                move |result| {
                    Message::Lsp(LspMsg::StartResult {
                        stamp: stamp.clone(),
                        language: lang_done.clone(),
                        generation,
                        result,
                    })
                },
            );
        }

        // Fallback: spawn the language server locally.
        let generation = self.next_lsp_gen(&lang);
        let stamp = self.transport_stamp();
        Task::perform(
            async move { lsp::client::LspClient::start(&exe, &args, &root, init).await },
            move |result| {
                Message::Lsp(LspMsg::StartResult {
                    stamp: stamp.clone(),
                    language: lang.clone(),
                    generation,
                    result,
                })
            },
        )
    }

    /// Mint the next spawn generation for `language`. Called at the start of
    /// every spawn/install; the previous generation's in-flight result becomes
    /// stale the moment this returns.
    pub(crate) fn next_lsp_gen(&mut self, language: &str) -> u64 {
        let g = self.lsp_gen.entry(language.to_string()).or_insert(0);
        *g += 1;
        *g
    }

    /// Forget everything the client remembers about `language`'s server, so the
    /// next `ensure_lsp` starts a fresh one. Shared by the explicit restart and
    /// by a server's death: the documents must be re-opened against whatever
    /// replaces it, any in-flight spawn of the old generation is superseded, and
    /// the diagnostic/inlay high-water marks have to go — the replacement counts
    /// from zero, so a stale mark would suppress every refetch forever.
    pub(crate) fn reset_lsp(&mut self, language: &str) {
        // Stops a Ready client (see `LspSlots`), and an install in flight.
        self.proj.link.lsp.remove(language);
        self.proj.link.lsp_installs.remove(language);
        self.proj
            .link
            .lsp_opened
            .retain(|p| highlight::detect(p) != Some(language));
        self.next_lsp_gen(language);
        self.proj.link.seen_diag_version.remove(language);
        self.proj.link.seen_inlay_epoch.remove(language);
    }

    /// Send `didOpen` for every loaded document of `language` not yet opened.
    pub(crate) fn open_docs_for_language(
        &mut self,
        language: &str,
        client: &lsp::client::LspClient,
    ) -> Task<Message> {
        let docs: Vec<(PathBuf, Arc<String>)> = self
            .proj
            .panes
            .iter()
            .flatten()
            .filter(|v| v.lang_key == Some(language))
            .map(|v| (v.abs.clone(), v.source.clone()))
            .collect();
        let mut tasks = Vec::new();
        for (path, source) in docs {
            if self.proj.link.lsp_opened.insert(path.clone()) {
                client.did_open(&path, language, 1, &source);
            }
            tasks.push(self.inlay_request(&path, client));
        }
        Task::batch(tasks)
    }

    /// Push `source` to the language server as the new content of `path`.
    ///
    /// The server owns its copy of a document from `didOpen` until a `didClose`
    /// clew never sends, and `open_docs_for_language` opens each path exactly
    /// once, so this is the ONLY thing that can bring the server's copy back in
    /// line with the file. Deliberately keyed on the document being open on the
    /// SERVER rather than on being on screen: a file changed while another file
    /// occupies its pane would otherwise leave definitions, references, hover
    /// and inlay hints resolving against the pre-change text for the rest of
    /// the session, since re-opening it sends nothing either. Callable from
    /// both the local rehash path and the remote refresh path — the whole
    /// reason it lives here rather than inline in one of them.
    pub(crate) fn resync_open_doc(&mut self, path: &Path, source: &str) {
        let Some(lang) = highlight::detect(path) else {
            return;
        };
        if !self.proj.link.lsp_opened.contains(path) {
            return;
        }
        let Some(LspSlot::Ready(client)) = self.proj.link.lsp.get(lang) else {
            return;
        };
        self.lsp_doc_rev += 1;
        client.did_change(path, self.lsp_doc_rev, source);
    }

    pub(crate) fn on_lsp_consent_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.proj.pending_lsp_consent.pop_front() else {
            return Task::none();
        };
        // What the consent was for decides the path, in ONE match — the local
        // installs below are the only ones left once it returns for the rest.
        let local = match c.provision {
            // A debug adapter, not a language server: its own install path.
            LspProvision::DebugAdapter { install, stamp } => {
                return self.on_debug_install_allowed(install, stamp);
            }
            // A remote install: the consent turns into an `LspInstall`
            // request — the ONLY message the server installs anything on. The
            // slot stays AwaitingConsent so the `LspResolved` reply (Ready on
            // success) is picked up by the same handler that started this
            // flow.
            LspProvision::Remote { consent, .. } => {
                self.proj
                    .link
                    .lsp
                    .insert(c.language.clone(), LspSlot::AwaitingConsent);
                if self.server.is_up() {
                    self.status = format!("Installing {} on the remote host…", c.server_name);
                    // Back verbatim: the consent covers the install the user
                    // saw, and the server runs no other.
                    let _ = self.send_to_server(clew_protocol::Request::LspInstall {
                        language: c.language,
                        consent,
                    });
                }
                return Task::none();
            }
            LspProvision::Download(download) => LocalInstall::Download(download),
            LspProvision::Install(install) => LocalInstall::Toolchain(install),
        };
        self.proj
            .link
            .lsp
            .insert(c.language.clone(), LspSlot::Starting);
        let (dest, language, version) = (c.dest_dir, c.language, c.version);
        // Mint this install's generation so a result landing after a restart
        // or project switch is recognized as superseded.
        let generation = self.next_lsp_gen(&language);
        let stamp = self.transport_stamp();
        // Its cancel guard, kept per language: a newer install of the
        // language (the insert below drops an older guard), a restart, or the
        // link going stops this one between chunks instead of letting it run
        // to its deadline for a result that would be dropped as superseded.
        let guard = RaiseOnDrop::default();
        let cancel = guard.flag();
        self.proj.link.lsp_installs.insert(language.clone(), guard);
        self.status = match &local {
            LocalInstall::Download(_) => format!("Downloading {}…", c.server_name),
            LocalInstall::Toolchain(_) => format!("Installing {}…", c.server_name),
        };
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || match local {
                    LocalInstall::Download(download) => {
                        lsp::store::download_and_install_cancellable(&download, &dest, &cancel)
                    }
                    LocalInstall::Toolchain(install) => lsp::store::toolchain_install_cancellable(
                        &install, &version, &dest, &cancel,
                    ),
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            move |result| {
                Message::Lsp(LspMsg::DownloadResult {
                    stamp: stamp.clone(),
                    language: language.clone(),
                    generation,
                    result,
                })
            },
        )
    }

    /// The user approved the language-server command this project's `lsp.toml`
    /// names. Record the approval against the fingerprint the modal SHOWED,
    /// then restart the resolve flow from scratch — never spawn what the
    /// modal remembered. The command file may have changed while the dialog
    /// sat open; re-entering `ensure_lsp` re-fingerprints the file as it is
    /// NOW, so a swapped script fails the approval check and raises a fresh
    /// modal instead of running.
    pub(crate) fn on_lsp_command_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.proj.pending_lsp_command.take() else {
            return Task::none();
        };
        // Record against the root/host the modal was raised for — never the
        // current project, which may have changed while the modal sat open.
        if self.proj.project.as_ref().map(|p| &p.root) != Some(&c.root)
            || self.connection.approval_host().map(str::to_string) != c.host
        {
            self.status = "The project changed — nothing was approved".into();
            return Task::none();
        }
        if let Err(e) = self
            .trust
            .update(|t| t.approve_lsp(c.host.as_deref(), &c.root, &c.language, &c.fingerprint))
        {
            // `Trust::update` adopts the change only once the file is written,
            // so a failure means NOTHING was approved — not on disk, not in
            // memory. Falling through to `ensure_lsp` then re-raised the very
            // same modal, and the loop had no exit but closing the project.
            // Report it in the slot instead, which offers a deliberate Retry.
            self.status = format!("Could not record the approval: {e}");
            self.proj.link.lsp.insert(
                c.language.clone(),
                LspSlot::Failed(format!("could not record the approval: {e}")),
            );
            return Task::none();
        }
        // The server enforces the same gate (SpawnLsp, the Ask agent's
        // semantic tools) — push the fresh approval before starting.
        self.send_lsp_approvals();
        self.proj.link.lsp.remove(&c.language);
        self.ensure_lsp(&c.language)
    }

    /// Push this project's language-server command approvals to the server,
    /// which enforces them on every spawn path. Replaces the server's set.
    pub(crate) fn send_lsp_approvals(&mut self) {
        let (Some(root), true) = (
            self.proj.project.as_ref().map(|p| &p.root),
            self.server.is_up(),
        ) else {
            return;
        };
        let approvals = self
            .trust
            .lsp_approvals_for(self.connection.approval_host(), root);
        let _ = self.send_to_server(clew_protocol::Request::LspApprovals { approvals });
    }

    /// Request whole-file inlay hints for `abs` from `client` (no-op unless the
    /// server advertised the capability). Whole-file, not per-viewport: simpler,
    /// and the server caches.
    pub(crate) fn inlay_request(
        &self,
        abs: &Path,
        client: &lsp::client::LspClient,
    ) -> Task<Message> {
        if !client.inlay_hint || !self.show_inlay_hints {
            return Task::none();
        }
        // Stamp the request with the bytes it is about, the way the
        // highlighting pass does. Two requests for one file can be in flight
        // and finish in either order.
        let Some((lines, src_hash)) = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == *abs)
            .map(|v| {
                (
                    v.lines.len(),
                    incremental::content_hash(v.source.as_bytes()),
                )
            })
        else {
            return Task::none();
        };
        let client = client.clone();
        let path = abs.to_path_buf();
        let tag = path.clone();
        let hint_gen = self.inlay_gen;
        let stamp = self.stamp();
        Task::perform(
            async move { client.inlay_hints(&path, 0, lines).await },
            move |hints| {
                Message::Lsp(LspMsg::InlayHintsLoaded {
                    stamp: stamp.clone(),
                    abs: tag.clone(),
                    hint_gen,
                    src_hash,
                    hints,
                })
            },
        )
    }

    /// Request inlay hints for `abs`, looking its language's server up in the
    /// registry (for callers that don't already hold the client).
    pub(crate) fn inlay_request_lookup(&self, abs: &Path) -> Task<Message> {
        let Some(lang) = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == *abs)
            .and_then(|v| v.lang_key)
        else {
            return Task::none();
        };
        match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(client)) => self.inlay_request(abs, client),
            _ => Task::none(),
        }
    }

    pub(crate) fn on_inlay_hints_loaded(
        &mut self,
        abs: PathBuf,
        hint_gen: u64,
        src_hash: incremental::Version,
        hints: Vec<lsp::client::InlayHint>,
    ) -> Task<Message> {
        // Both halves matter. The toggle may be off right now, or it may have
        // been turned off and on again since this request went out — in which
        // case these hints describe the earlier period and a fresher batch is
        // already coming.
        if !self.show_inlay_hints || hint_gen != self.inlay_gen {
            return Task::none();
        }
        // The server's position encoding, for mapping its character offsets
        // to display columns (tabs already expanded to 4). LSP's default,
        // UTF-16, when the server has since gone.
        let enc = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == abs)
            .and_then(|v| v.lang_key)
            .and_then(|l| match self.proj.link.lsp.get(l) {
                Some(LspSlot::Ready(c)) => Some(c.encoding),
                _ => None,
            })
            .unwrap_or(lsp::client::PositionEncoding::Utf16);
        for slot in &mut self.proj.panes {
            // Applied only to a pane still showing the exact bytes the server
            // computed these hints for. `hint_gen` alone could not tell: it
            // moves on a TOGGLE, never on an edit, so an in-flight reply for
            // the pre-edit file was accepted and every chip landed on the
            // wrong token. Same test the highlighting pass makes.
            let Some(v) = slot
                .as_mut()
                .filter(|v| v.abs == abs)
                .filter(|v| incremental::content_hash(v.source.as_bytes()) == src_hash)
            else {
                continue;
            };
            let source = v.source.clone();
            let src_lines: Vec<&str> = source.lines().collect();
            let mut map: HashMap<usize, Vec<(usize, String)>> = HashMap::new();
            for h in &hints {
                let Some(line) = src_lines.get(h.line) else {
                    continue;
                };
                let col = viewer::Col::from_offset(line, h.character, enc).0;
                let mut text = h.label.clone();
                if h.padding_left {
                    text.insert(0, ' ');
                }
                if h.padding_right {
                    text.push(' ');
                }
                map.entry(h.line).or_default().push((col, text));
            }
            for chips in map.values_mut() {
                chips.sort_by_key(|(c, _)| *c);
            }
            v.set_inlay_hints(map);
        }
        Task::none()
    }

    pub(crate) fn on_toggle_inlay_hints(&mut self) -> Task<Message> {
        self.show_inlay_hints = !self.show_inlay_hints;
        self.show_tools_menu = false;
        if self.show_inlay_hints {
            // Re-fetch for every shown file.
            let files: Vec<PathBuf> = self
                .proj
                .panes
                .iter()
                .flatten()
                .map(|v| v.abs.clone())
                .collect();
            let tasks: Vec<Task<Message>> = files
                .iter()
                .map(|abs| self.inlay_request_lookup(abs))
                .collect();
            Task::batch(tasks)
        } else {
            // Clear so the hints disappear immediately, and invalidate every
            // request still in flight. Clearing alone was not enough: a reply
            // already on its way repopulated the hints a moment later, so the
            // toggle read "off" while the hints stayed on screen.
            self.inlay_gen += 1;
            for v in self.proj.panes.iter_mut().flatten() {
                v.set_inlay_hints(Default::default());
            }
            Task::none()
        }
    }

    /// Status text and the action button for a language row in the server
    /// panel. Distinguishes running / installed-but-idle / not-downloaded so an
    /// installed server never shows a misleading "Download".
    pub fn lsp_row(&self, language: &str) -> (String, Option<(&'static str, Message)>) {
        let restart = || {
            Some((
                "Restart",
                Message::Lsp(LspMsg::Restart(language.to_string())),
            ))
        };
        let provision = |label: &'static str| {
            Some((
                label,
                Message::Lsp(LspMsg::DownloadFor(language.to_string())),
            ))
        };
        match self.proj.link.lsp.get(language) {
            // A slot still holding a client whose transport is gone must not
            // read "ready": that lie is what stops the user from restarting a
            // server whose every request now fails.
            Some(LspSlot::Ready(c)) if !c.alive() => {
                ("stopped · restarts on open".into(), restart())
            }
            // Progress, or the server's complaint — including a state clew
            // can no longer read — from this update's snapshot.
            Some(slot @ LspSlot::Ready(_)) => (
                slot.label(self.proj.link.lsp_snapshots.get(language)),
                restart(),
            ),
            Some(LspSlot::Starting) => ("starting…".into(), None),
            Some(LspSlot::Failed(e)) => (format!("error: {e}"), provision("Retry")),
            Some(LspSlot::Unsupported(e)) => (e.clone(), None),
            Some(LspSlot::AwaitingConsent) => ("download pending".into(), provision("Download")),
            // Where the server would come from is a store probe on disk. The
            // panel is redrawn on every refresh tick, so the answer comes from
            // the listing the panel last ran off the UI thread
            // (`refresh_server_panel`), never from probing here per frame.
            None => match self.lsp_located.get(language) {
                Some(LocatedKind::Ready) => {
                    ("installed · starts on open".into(), provision("Start"))
                }
                Some(LocatedKind::NeedsDownload) => {
                    ("not downloaded".into(), provision("Download"))
                }
                Some(LocatedKind::NeedsInstall) => ("not installed".into(), provision("Install")),
                Some(LocatedKind::Unsupported(m)) => (m.clone(), None),
                Some(LocatedKind::NoServer) => ("no server".into(), None),
                None => ("checking…".into(), None),
            },
        }
    }

    /// (Re)list the Language Servers panel's contents on the blocking pool:
    /// the installed servers with their on-disk sizes (a recursive walk of the
    /// store) and, per language the panel shows, where its server would come
    /// from — for THIS project instance (its lsp.toml, its host), which the
    /// listing is stamped with. Lands as `LspMsg::PanelListed`.
    pub(crate) fn refresh_server_panel(&mut self) -> Task<Message> {
        self.server_panel_seq += 1;
        let seq = self.server_panel_seq;
        let stamp = self.stamp();
        // The config is resolved here (in memory) for every language a row can
        // show — the project's, the running ones, and every registry language,
        // since installed servers add rows the listing below discovers. Only
        // the store probes run on the blocking pool.
        let mut languages = self.managed_languages();
        languages.extend(
            lsp::registry::all()
                .iter()
                .flat_map(|spec| spec.languages.iter().map(|l| l.to_string())),
        );
        languages.sort();
        languages.dedup();
        let resolved: Vec<(String, Option<lsp::config::EffectiveServer>)> = languages
            .into_iter()
            .map(|language| {
                let server = self.proj.lsp_config.resolve(&language);
                (language, server)
            })
            .collect();
        let local = !self.connection.is_remote();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let installed = lsp::store::installed_servers();
                    let located = resolved
                        .into_iter()
                        .map(|(language, server)| {
                            // A remote project's servers run on the remote
                            // host; this machine's store says nothing about it.
                            let kind = match server {
                                Some(_) if !local => {
                                    LocatedKind::Unsupported("managed on the remote host".into())
                                }
                                Some(server) => match lsp::store::locate(&server) {
                                    lsp::store::Located::Ready(_)
                                    | lsp::store::Located::RepoCommand(_) => LocatedKind::Ready,
                                    lsp::store::Located::NeedsDownload { .. } => {
                                        LocatedKind::NeedsDownload
                                    }
                                    lsp::store::Located::NeedsInstall { .. } => {
                                        LocatedKind::NeedsInstall
                                    }
                                    lsp::store::Located::Unsupported(m) => {
                                        LocatedKind::Unsupported(m)
                                    }
                                },
                                None => LocatedKind::NoServer,
                            };
                            (language, kind)
                        })
                        .collect();
                    (installed, located)
                })
                .await
                .unwrap_or_default()
            },
            move |(installed, located)| {
                Message::Lsp(LspMsg::PanelListed {
                    stamp: stamp.clone(),
                    seq,
                    installed,
                    located,
                })
            },
        )
    }

    /// Languages to show in the server panel: those present in the project,
    /// plus any installed or running server (so they can be managed anywhere).
    pub fn managed_languages(&self) -> Vec<String> {
        let mut langs = self.proj.project_languages.clone();
        for srv in &self.installed_servers {
            if let Some(spec) = lsp::registry::by_name(&srv.name) {
                langs.extend(spec.languages.iter().map(|l| l.to_string()));
            }
        }
        langs.extend(self.proj.link.lsp.keys().cloned());
        langs.sort();
        langs.dedup();
        langs
    }

    pub(crate) fn on_lsp_remove(&mut self, name: String, version: String) -> Task<Message> {
        // Stop any running instance of this server first.
        let langs: Vec<String> = lsp::registry::by_name(&name)
            .map(|s| s.languages.iter().map(|l| l.to_string()).collect())
            .unwrap_or_default();
        for lang in langs {
            self.proj.link.lsp.remove(&lang);
        }
        // Deleting an install is a recursive remove of a large tree: on the
        // blocking pool, with the panel re-listed when it is done.
        self.status = format!("Removing {name} {version}…");
        Task::perform(
            {
                let (name, version) = (name.clone(), version.clone());
                async move {
                    tokio::task::spawn_blocking(move || {
                        lsp::store::remove(&name, &version).map(|_| ())
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
                }
            },
            move |result| {
                Message::Lsp(LspMsg::Removed {
                    name: name.clone(),
                    version: version.clone(),
                    result,
                })
            },
        )
    }
}

impl App {
    /// Handle a [`LspMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_lsp(&mut self, message: LspMsg) -> Task<Message> {
        match message {
            LspMsg::InlayHintsLoaded {
                abs,
                hint_gen,
                src_hash,
                hints,
                ..
            } => self.on_inlay_hints_loaded(abs, hint_gen, src_hash, hints),
            LspMsg::StartResult {
                language,
                generation,
                result,
                ..
            } => {
                // A result from a superseded spawn (restart, project switch)
                // would install a dead client as Ready; its process was
                // already killed by the newer spawn, so just drop it.
                if self.lsp_gen.get(&language).copied() != Some(generation) {
                    return Task::none();
                }
                match result {
                    Ok(client) => {
                        // Open every already-loaded document of this language.
                        let open_task = self.open_docs_for_language(&language, &client);
                        self.proj.link.lsp.insert(language, LspSlot::Ready(client));
                        open_task
                    }
                    Err(e) => {
                        self.status = format!("{language} server failed: {e}");
                        self.proj.link.lsp.insert(language, LspSlot::Failed(e));
                        Task::none()
                    }
                }
            }
            LspMsg::ConsentDismissed => {
                if let Some(c) = self.proj.pending_lsp_consent.pop_front() {
                    // A declined debug-adapter install is not a language
                    // server: there is no LSP slot to mark.
                    if let LspProvision::DebugAdapter { install, .. } = &c.provision {
                        self.status = format!("{} not installed", install.name());
                        return Task::none();
                    }
                    self.proj.link.lsp.insert(
                        c.language,
                        LspSlot::Unsupported("server download declined".into()),
                    );
                }
                Task::none()
            }
            LspMsg::ConsentAllowed => self.on_lsp_consent_allowed(),
            LspMsg::CommandAllowed => self.on_lsp_command_allowed(),
            LspMsg::CommandDismissed => {
                if let Some(c) = self.proj.pending_lsp_command.take() {
                    // Name what was actually declined: the modal asks about a
                    // command, about `initialize` options, or about both, and
                    // a slot that always said "command" left the user of an
                    // options-only config looking for one that is not there.
                    let what = match c.command.is_some() {
                        true => "command",
                        false => "initialize options",
                    };
                    self.proj.link.lsp.insert(
                        c.language,
                        LspSlot::Unsupported(format!("project's language-server {what} declined")),
                    );
                }
                Task::none()
            }
            LspMsg::DownloadResult {
                language,
                generation,
                result,
                ..
            } => {
                // Same staleness rule as `LspMsg::StartResult`: an install that began
                // for another project (or before a restart) must not start a
                // server here.
                if self.lsp_gen.get(&language).copied() != Some(generation) {
                    return Task::none();
                }
                // Finished: nothing left for its guard to stop.
                self.proj.link.lsp_installs.remove(&language);
                match result {
                    Ok(exe) => {
                        self.status = format!("{language} server installed");
                        self.start_lsp_with(&language, exe)
                    }
                    Err(e) => {
                        self.status = format!("{language} server download failed: {e}");
                        self.proj.link.lsp.insert(language, LspSlot::Failed(e));
                        Task::none()
                    }
                }
            }
            LspMsg::Staged {
                stamp,
                language,
                generation,
                result,
            } => match stamp.root {
                // Owned (checked in `dispatch`), so this is the open project's root.
                Some(root) => self.on_lsp_staged(language, generation, root, result),
                None => Task::none(),
            },
            LspMsg::PanelListed {
                seq,
                installed,
                located,
                ..
            } => {
                // (A listing made for a project instance this window has left
                // was dropped in `dispatch`, by its stamp.) A listing a newer
                // one superseded (the panel was refreshed, a server was
                // removed) must not paint over it.
                if seq == self.server_panel_seq {
                    self.installed_servers = installed;
                    self.lsp_located = located;
                }
                Task::none()
            }
            LspMsg::Removed {
                name,
                version,
                result,
            } => {
                self.status = match result {
                    Ok(()) => format!("Removed {name} {version}"),
                    Err(e) => format!("Remove failed: {e}"),
                };
                self.refresh_server_panel()
            }
            LspMsg::ToggleInlayHints => self.on_toggle_inlay_hints(),
            LspMsg::TogglePanel => {
                self.server_panel = !self.server_panel;
                if self.server_panel {
                    // Sizing every installed server walks the store's whole
                    // tree: done on the blocking pool, painted when it lands.
                    return self.refresh_server_panel();
                }
                Task::none()
            }
            LspMsg::Restart(language) => {
                // Drop the running server (kills its child), then re-provision.
                self.reset_lsp(&language);
                self.ensure_lsp(&language)
            }
            LspMsg::Remove { name, version } => self.on_lsp_remove(name, version),
            LspMsg::DownloadFor(language) => {
                // Force a fresh provisioning attempt for this language.
                self.proj.link.lsp.remove(&language);
                self.ensure_lsp(&language)
            }
        }
    }
}
