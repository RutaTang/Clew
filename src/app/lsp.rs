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
        text.push('\n');
        text.push_str(tr!(
            "… truncated for display — the approval covers the whole file's options"
        ));
    }
    Some(text)
}

/// A language-server install that runs on THIS machine, once the user
/// consented to it (see `App::on_lsp_consent_allowed`).
pub(crate) enum LocalInstall {
    Download(lsp::registry::Download),
    Toolchain(lsp::registry::Install),
}

/// Most inlay chips one line carries, and most chars they add up to — a call
/// with a hundred arguments has a hundred, far shorter: a server's reply may
/// hold any number for one line, and each is shaped with it.
pub(crate) const MAX_LINE_CHIPS: usize = 256;
pub(crate) const MAX_LINE_CHIP_CHARS: usize = 4096;

impl App {
    /// Whether `language`'s server is left "not set up" when it would ask:
    /// the first open left the servers for later, and the reader has not
    /// asked for this one since ("Ask again" asks about that one only).
    pub(crate) fn server_left_for_later(&self, language: &str) -> bool {
        self.proj.servers_not_now && !self.proj.servers_asked.contains(language)
    }

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
                        // Diagnostics on their way from a server that says
                        // no more of its work: read when they come.
                        snap.progress.is_some()
                            || snap.awaiting_diagnostics
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

        // The user's choices for the project (its Python interpreter) are
        // still being read: a server started now would not have them.
        if self.proj.inflight.choices_loading {
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
                    choices: self.proj.lang_choices.clone(),
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
        let Some(server) = self.effective_server(language) else {
            self.proj.link.lsp.insert(
                language.to_string(),
                LspSlot::Unsupported(tr_noop!("no server for this language").into()),
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
        if self.server_left_for_later(language) {
            self.leave_server_for_later(language);
            return Task::none();
        }
        self.proj
            .link
            .lsp
            .insert(language.to_string(), LspSlot::AwaitingConsent);
        // The server's own program already on this computer is offered in
        // place of fetching Clew's copy — once it is seen to run, off the UI
        // thread: the question is asked meanwhile, and asked again with the
        // offer (`LspMsg::FoundOnPath`).
        // Not for a server clew's own install keeps from running the
        // project's code (typescript-language-server, whose TypeScript it
        // pins): the user's program in its place would run the
        // repository's `tsserver.js`, one click from a question that says
        // nothing of it.
        let name = path_name(&provision).filter(|_| !lsp::langenv::pins(&server.server_name));
        // Looked for in the task (a Finder-opened app's PATH is the login
        // shell's, asked once and maybe not yet answered), on the PATH a
        // test set for this thread when it set one.
        let path = path_set_here();
        let fetched = provision.clone();
        self.proj.pending_lsp_consent.offer(LspConsent {
            language: language.to_string(),
            server_name: server.server_name,
            version: server.version,
            provision,
            dest_dir,
            on_path: None,
        });
        let Some(name) = name else {
            return Task::none();
        };
        let stamp = self.transport_stamp();
        let language = language.to_string();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let path = path.unwrap_or_else(clew_core::linerpc::child_path);
                    on_path_for(&fetched, Some(&path))
                        .filter(|program| worth_offering(program, &path))
                })
                .await
                .ok()
                .flatten()
            },
            move |runs| match runs {
                Some(program) => Message::Lsp(LspMsg::FoundOnPath {
                    stamp: stamp.clone(),
                    language: language.clone(),
                    name,
                    program,
                }),
                None => Message::Noop,
            },
        )
    }

    /// The project was opened without its servers set up: `language`'s is
    /// not asked about, and stays "not set up" — the status bar offers to
    /// set it up, the Languages page to ask again.
    fn leave_server_for_later(&mut self, language: &str) {
        self.proj.link.lsp.insert(
            language.to_string(),
            LspSlot::Declined(tr_noop!("not set up").into()),
        );
    }

    /// The program the download question for `language` would fetch was
    /// found on PATH and runs: the question, if it is still waiting, is
    /// asked again offering it — under a number of its own, so the new
    /// button is read before it counts.
    pub(crate) fn on_lsp_found_on_path(
        &mut self,
        language: String,
        name: &'static str,
        program: PathBuf,
    ) {
        let Some(mut consent) = self
            .proj
            .pending_lsp_consent
            .iter()
            .find(|c| c.language == language && path_name(&c.provision) == Some(name))
            .cloned()
        else {
            return;
        };
        if consent.on_path.as_deref() == Some(program.as_path()) {
            return;
        }
        consent.on_path = Some(program);
        self.proj.pending_lsp_consent.offer(consent);
    }

    /// The download prompt's "Use that one": the program found on PATH is
    /// chosen as the language's server — as Settings → Language servers
    /// chooses one — and started, nothing fetched.
    pub(crate) fn on_lsp_consent_use_path(&mut self, number: u64) -> Task<Message> {
        if !self.may_allow(&crate::app::model::Question::LspConsent(number)) {
            return Task::none();
        }
        let Some(c) = self.proj.pending_lsp_consent.answer(number) else {
            return Task::none();
        };
        let Some(program) = c.on_path else {
            return Task::none();
        };
        self.use_server_program(&c.language, &program)
    }

    /// `program`, the server's own on this computer, chosen for
    /// `language` in the project — and started, out of any slot it waited
    /// in.
    pub(crate) fn use_server_program(
        &mut self,
        language: &str,
        program: &std::path::Path,
    ) -> Task<Message> {
        let mut next = self.proj.lang_choices.clone();
        next.servers.get_or_insert_with(Default::default).insert(
            language.to_string(),
            clew_protocol::ServerChoice {
                command: program.to_string_lossy().into_owned(),
                args: None,
            },
        );
        // Out of the slot it waited in, so the choice starts it afresh.
        self.proj.link.lsp.remove(language);
        let chosen = self.choose(next);
        Task::batch([chosen, self.ensure_lsp(language)])
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
                    Ok(Box::new(LspStagedCommand {
                        fingerprint,
                        source,
                        exec_path,
                        server,
                    }))
                })
                .await
                .unwrap_or_else(|e| {
                    Err(tr!(
                        "preparing the language server command failed: {error}",
                        error = e
                    ))
                })
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
        result: Result<Box<LspStagedCommand>, String>,
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
        if self.effective_server(&language).as_ref() != Some(&staged.server) {
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
            self.proj.pending_lsp_command.offer(PendingLspCommand {
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
    /// interpreter through `foreign_bytes` precisely so repository bytes
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
        self.proj.pending_lsp_command.offer(PendingLspCommand {
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
            let Some(server) = self.effective_server(language) else {
                return Task::none();
            };
            // A server that builds the project runs the repository's own code
            // on load: it starts only once the user has let it build THIS
            // project. Asked here, where every local start arrives, for the
            // reason the options below are. (A remote host asks its own
            // question, in its `LspResolve` answer and again at `SpawnLsp`.)
            if let Some(runs) = server.runs_project_code()
                && !self
                    .trust
                    .is_build_trusted(None, &root, &server.server_name)
            {
                if self.server_left_for_later(language) {
                    self.leave_server_for_later(language);
                    return Task::none();
                }
                self.proj
                    .link
                    .lsp
                    .insert(language.to_string(), LspSlot::AwaitingConsent);
                self.proj.pending_lsp_consent.offer(LspConsent {
                    on_path: None,
                    language: language.to_string(),
                    server_name: server.server_name,
                    version: server.version,
                    provision: LspProvision::Build {
                        runs: runs.to_string(),
                        root,
                        host: None,
                    },
                    dest_dir: PathBuf::new(),
                });
                return Task::none();
            }
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
            // (explicit wins), over the settings clew pins for its own
            // binary — which, when they cannot be pinned, keep it stopped.
            let init = match langenv::merge(
                language,
                &server,
                &exe,
                &root,
                explicit,
                &self.proj.lang_choices,
            ) {
                Ok(init) => init,
                Err(e) => {
                    self.proj
                        .link
                        .lsp
                        .insert(language.to_string(), LspSlot::Failed(e));
                    return Task::none();
                }
            };
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
                    choices: self.proj.lang_choices.clone(),
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
        // One the host runs is no longer waited for: what is asked next is
        // asked anew.
        self.proj.link.lsp.remove(language);
        self.proj.link.lsp_installs.remove(language);
        self.proj.link.remote_installs.remove(language);
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

    /// The user allowed the consent question `number`, if it is the one
    /// within reach and has been long enough to be read
    /// ([`App::may_allow`]).
    pub(crate) fn on_lsp_consent_allowed(&mut self, number: u64) -> Task<Message> {
        if !self.may_allow(&crate::app::model::Question::LspConsent(number)) {
            return Task::none();
        }
        let Some(c) = self.proj.pending_lsp_consent.answer(number) else {
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
                    self.status = tr!(
                        "Installing {name} on the remote host…",
                        name = c.server_name
                    );
                    // Back verbatim: the consent covers the install the user
                    // saw, and the server runs no other.
                    let sent = self.send_to_server(clew_protocol::Request::LspInstall {
                        language: c.language.clone(),
                        consent,
                        choices: self.proj.lang_choices.clone(),
                    });
                    if let Some(id) = sent {
                        self.proj.link.remote_installs.insert(c.language, id);
                    }
                }
                return Task::none();
            }
            LspProvision::Build { root, host, .. } => {
                return self.on_build_allowed(&c.language, &c.server_name, root, host);
            }
            LspProvision::Download(download) => LocalInstall::Download(download),
            LspProvision::Install(install) => LocalInstall::Toolchain(install),
        };
        self.proj
            .link
            .lsp
            .insert(c.language.clone(), LspSlot::Starting);
        self.install_lsp(
            c.language,
            c.server_name,
            c.version,
            local,
            c.dest_dir,
            true,
        )
    }

    /// Download or install `language`'s server (`server_name` at `version`)
    /// into `dest`, off the UI thread: started when it is in (`start`), or
    /// left in the store for when a file of the language opens
    /// ([`LspMsg::Fetched`], a fetch ahead of need).
    pub(crate) fn install_lsp(
        &mut self,
        language: String,
        server_name: String,
        version: String,
        local: LocalInstall,
        dest: PathBuf,
        start: bool,
    ) -> Task<Message> {
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
            LocalInstall::Download(_) => tr!("Downloading {name}…", name = server_name),
            LocalInstall::Toolchain(_) => tr!("Installing {name}…", name = server_name),
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
                let (stamp, language) = (stamp.clone(), language.clone());
                Message::Lsp(if start {
                    LspMsg::DownloadResult {
                        stamp,
                        language,
                        generation,
                        result,
                    }
                } else {
                    LspMsg::Fetched {
                        stamp,
                        language,
                        generation,
                        result,
                    }
                })
            },
        )
    }

    /// The user let `server` build the project `root` on `host`: record it,
    /// outside the project, and start the language afresh. Recorded against the
    /// project the question was asked about — never the current one, which may
    /// have changed while the modal sat open.
    fn on_build_allowed(
        &mut self,
        language: &str,
        server: &str,
        root: PathBuf,
        host: Option<String>,
    ) -> Task<Message> {
        if self.proj.project.as_ref().map(|p| &p.root) != Some(&root)
            || self.connection.approval_host().map(str::to_string) != host
        {
            self.status = tr!("The project changed — nothing was allowed").into();
            return Task::none();
        }
        if let Err(e) = self
            .trust
            .update(|t| t.trust_build(host.as_deref(), &root, server))
        {
            // Nothing was recorded (see `on_lsp_command_allowed`): a slot that
            // says so, rather than the same question again.
            self.status = tr!("Could not save permission to build: {error}", error = e);
            self.proj.link.lsp.insert(
                language.to_string(),
                LspSlot::Failed(tr!(
                    "could not save permission to build: {error}",
                    error = e
                )),
            );
            return Task::none();
        }
        // The server holds the same gate (SpawnLsp, the agent's semantic
        // tools, a remote's resolve): it hears of the leave first.
        self.send_lsp_approvals();
        self.proj.link.lsp.remove(language);
        self.ensure_lsp(language)
    }

    /// The user approved the language-server command this project's `lsp.toml`
    /// names. Record the approval against the fingerprint the modal SHOWED,
    /// then restart the resolve flow from scratch — never spawn what the
    /// modal remembered. The command file may have changed while the dialog
    /// sat open; re-entering `ensure_lsp` re-fingerprints the file as it is
    /// NOW, so a swapped script fails the approval check and raises a fresh
    /// modal instead of running.
    ///
    /// Only the question `number`, and only once it has been within reach
    /// long enough to be read ([`App::may_allow`]).
    pub(crate) fn on_lsp_command_allowed(&mut self, number: u64) -> Task<Message> {
        if !self.may_allow(&crate::app::model::Question::LspCommand(number)) {
            return Task::none();
        }
        let Some(c) = self.proj.pending_lsp_command.answer(number) else {
            return Task::none();
        };
        // Record against the root/host the modal was raised for — never the
        // current project, which may have changed while the modal sat open.
        if self.proj.project.as_ref().map(|p| &p.root) != Some(&c.root)
            || self.connection.approval_host().map(str::to_string) != c.host
        {
            self.status = tr!("The project changed — nothing was approved").into();
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
            self.status = tr!("Could not record the approval: {error}", error = e);
            self.proj.link.lsp.insert(
                c.language.clone(),
                LspSlot::Failed(tr!("could not record the approval: {error}", error = e)),
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
        let _ = self.send_to_server(clew_protocol::Request::LspApprovals {
            root: root.to_string_lossy().into_owned(),
            approvals,
        });
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
            // The chars of each line's chips so far.
            let mut spliced: HashMap<usize, usize> = HashMap::new();
            for h in &hints {
                let Some(line) = src_lines.get(h.line) else {
                    continue;
                };
                // Past what the code view shapes of a line, a chip is never
                // drawn: no char takes more than four units of any encoding,
                // so a hint that far in is at a column past the cut — and
                // finding its column walked the line up to it, per chip.
                if h.character / 4 > crate::codeview::MAX_SHAPED_COLS {
                    continue;
                }
                // Every chip is spliced into its line, shaped with it on the
                // UI thread: what one reply may put on a line is bounded
                // before anything is computed for it.
                let chips = map.entry(h.line).or_default();
                let chars = spliced.entry(h.line).or_default();
                let len = h.label.chars().count() + 2;
                if chips.len() >= MAX_LINE_CHIPS || *chars + len > MAX_LINE_CHIP_CHARS {
                    continue;
                }
                *chars += len;
                let col = viewer::Col::from_offset(line, h.character, enc).0;
                let mut text = h.label.clone();
                if h.padding_left {
                    text.insert(0, ' ');
                }
                if h.padding_right {
                    text.push(' ');
                }
                chips.push((col, text));
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
        self.keep_view_pref(crate::app::runtime::VIEW_INLAY_HINTS, self.show_inlay_hints);
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

    /// What `language`'s [`LspSlot::AwaitingConsent`] slot waits on.
    pub fn lsp_awaiting(&self, language: &str) -> Awaiting {
        let consent = self
            .proj
            .pending_lsp_consent
            .iter()
            .find(|c| c.language == language);
        match consent.map(|c| &c.provision) {
            Some(LspProvision::Build { .. }) => Awaiting::Build,
            Some(_) => Awaiting::Download,
            None if self
                .proj
                .pending_lsp_command
                .iter()
                .any(|c| c.language == language) =>
            {
                Awaiting::Approval
            }
            None if self.proj.link.remote_installs.contains_key(language) => Awaiting::Installing,
            None if self.connection.is_remote() => Awaiting::Host,
            None => Awaiting::Nothing,
        }
    }

    /// The status bar's word on `language`'s server, from the snapshot this
    /// update took.
    pub fn lsp_status(&self, language: &str) -> Option<String> {
        let slot = self.proj.link.lsp.get(language)?;
        Some(match slot {
            LspSlot::AwaitingConsent => self.lsp_awaiting(language).to_string(),
            slot => slot.label(self.proj.link.lsp_snapshots.get(language)),
        })
    }

    /// Whether `language`'s server is at work — starting, or saying what it
    /// is doing (indexing, building) — in words that change as it goes.
    pub fn lsp_busy(&self, language: &str) -> bool {
        match self.proj.link.lsp.get(language) {
            Some(LspSlot::Starting) => true,
            Some(LspSlot::Ready(_)) => matches!(
                self.proj.link.lsp_snapshots.get(language),
                Some(Ok(s)) if s.error.is_none() && s.progress.is_some()
            ),
            _ => false,
        }
    }

    /// Status text and the action button for a language row in the server
    /// panel. Distinguishes running / installed-but-idle / not-downloaded so an
    /// installed server never shows a misleading "Download".
    pub fn lsp_row(&self, language: &str) -> (String, Option<(&'static str, Message)>) {
        let restart = || {
            Some((
                tr!("Restart"),
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
                (tr!("stopped, restarts on open").into(), restart())
            }
            // Progress, or the server's complaint — including a state clew
            // can no longer read — from this update's snapshot.
            Some(slot @ LspSlot::Ready(_)) => (
                slot.label(self.proj.link.lsp_snapshots.get(language)),
                restart(),
            ),
            Some(LspSlot::Starting) => (tr!("starting…").into(), None),
            Some(LspSlot::Failed(e)) => (tr!("error: {error}", error = e), provision(tr!("Retry"))),
            Some(LspSlot::Unsupported(e)) => (crate::i18n::message(e), None),
            // A "no" is not for good: without a way to ask again, the
            // language had no server until the project was reopened.
            Some(LspSlot::Declined(e)) => (crate::i18n::message(e), provision(tr!("Ask again"))),
            // The question waiting may be leave to build, or the project's
            // command, rather than an install: nothing to download, and no
            // button that reads so.
            Some(LspSlot::AwaitingConsent) => match self.lsp_awaiting(language) {
                Awaiting::Download => (tr!("download pending").into(), provision(tr!("Download"))),
                // Nothing on screen to answer — the host's word lost with a
                // link, a question that went with it — or not yet: asked
                // again, the start resolves afresh.
                waiting @ (Awaiting::Host | Awaiting::Nothing) => {
                    (waiting.to_string(), provision(tr!("Ask again")))
                }
                other => (other.to_string(), None),
            },
            // Where the server would come from is a store probe on disk. The
            // panel is redrawn on every refresh tick, so the answer comes from
            // the listing the panel last ran off the UI thread
            // (`refresh_server_panel`), never from probing here per frame.
            None => match self.lsp_located.get(language) {
                Some(LocatedKind::Ready) => (
                    tr!("installed, starts on open").into(),
                    provision(tr!("Start")),
                ),
                Some(LocatedKind::NeedsDownload) => {
                    (tr!("not downloaded").into(), provision(tr!("Download")))
                }
                Some(LocatedKind::NeedsInstall) => {
                    (tr!("not installed").into(), provision(tr!("Install")))
                }
                Some(LocatedKind::Unsupported(m)) => (crate::i18n::message(m), None),
                Some(LocatedKind::NoServer) => (tr!("no server").into(), None),
                None => (tr!("checking…").into(), None),
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
                let server = self.effective_server(&language);
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
                                Some(_) if !local => LocatedKind::Unsupported(
                                    tr!("managed on the remote host").into(),
                                ),
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
        // Stop any running instance of this server first — with all that a
        // stop resets: dropping only the slot left its documents marked as
        // opened, so the server the next need starts was never sent their
        // `didOpen`, and a spawn still in flight was not superseded.
        let langs: Vec<String> = lsp::registry::by_name(&name)
            .map(|s| s.languages.iter().map(|l| l.to_string()).collect())
            .unwrap_or_default();
        for lang in langs {
            self.reset_lsp(&lang);
        }
        // Deleting an install is a recursive remove of a large tree: on the
        // blocking pool, with the panel re-listed when it is done.
        self.status = tr!("Removing {name} {version}…", name = name, version = version);
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
                        self.status = tr!(
                            "{language} server failed: {error}",
                            language = crate::app::languages::language_name(&language),
                            error = e
                        );
                        self.proj.link.lsp.insert(language, LspSlot::Failed(e));
                        Task::none()
                    }
                }
            }
            LspMsg::ConsentDismissed(number) => {
                if let Some(c) = self.proj.pending_lsp_consent.answer(number) {
                    // A declined debug-adapter install is not a language
                    // server: there is no LSP slot to mark.
                    if let LspProvision::DebugAdapter { install, .. } = &c.provision {
                        self.status = tr!("{name} not installed", name = install.name());
                        return Task::none();
                    }
                    let declined = match c.provision {
                        LspProvision::Build { .. } => tr_noop!("not allowed to build this project"),
                        _ => tr_noop!("server download declined"),
                    };
                    self.proj
                        .link
                        .lsp
                        .insert(c.language, LspSlot::Declined(declined.into()));
                }
                Task::none()
            }
            LspMsg::ConsentAllowed(number) => self.on_lsp_consent_allowed(number),
            LspMsg::ConsentUsePath(number) => self.on_lsp_consent_use_path(number),
            LspMsg::FoundOnPath {
                language,
                name,
                program,
                ..
            } => {
                self.on_lsp_found_on_path(language, name, program);
                Task::none()
            }
            LspMsg::CommandAllowed(number) => self.on_lsp_command_allowed(number),
            LspMsg::CommandDismissed(number) => {
                if let Some(c) = self.proj.pending_lsp_command.answer(number) {
                    // Name what was actually declined: the modal asks about a
                    // command, about `initialize` options, or about both, and
                    // a slot that always said "command" left the user of an
                    // options-only config looking for one that is not there.
                    let declined = match c.command.is_some() {
                        true => tr_noop!("project's language-server command declined"),
                        false => tr_noop!("project's language-server initialize options declined"),
                    };
                    self.proj
                        .link
                        .lsp
                        .insert(c.language, LspSlot::Declined(declined.into()));
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
                let look = self.look_for_missing_setup();
                match result {
                    Ok(exe) => {
                        self.status = tr!(
                            "{language} server installed",
                            language = crate::app::languages::language_name(&language)
                        );
                        Task::batch([self.start_lsp_with(&language, exe), look])
                    }
                    Err(e) => {
                        self.status = tr!(
                            "{language} server download failed: {error}",
                            language = crate::app::languages::language_name(&language),
                            error = e
                        );
                        self.proj.link.lsp.insert(language, LspSlot::Failed(e));
                        look
                    }
                }
            }
            LspMsg::Fetched {
                language,
                generation,
                result,
                ..
            } => {
                // An install begun for another project, or before a restart,
                // is not this one's.
                if self.lsp_gen.get(&language).copied() != Some(generation) {
                    return Task::none();
                }
                self.proj.link.lsp_installs.remove(&language);
                let fetched = self.on_lsp_fetched(language, result);
                Task::batch([fetched, self.look_for_missing_setup()])
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
                    Ok(()) => tr!("Removed {name} {version}", name = name, version = version),
                    Err(e) => tr!("Remove failed: {error}", error = e),
                };
                // A server the project needs, gone: offered to set up again.
                Task::batch([self.refresh_server_panel(), self.look_for_missing_setup()])
            }
            LspMsg::ToggleInlayHints => self.on_toggle_inlay_hints(),
            LspMsg::TogglePanel => {
                // The servers are a page of Settings — Languages — not a
                // panel apart. Showing it lists them (`sync_server_listing`:
                // sizing every installed server walks the store's whole tree,
                // on the blocking pool, painted when it lands).
                if self.settings.open && self.settings.page == crate::SettingsPage::Languages {
                    return self.close_settings();
                }
                self.update(Message::Settings(crate::SettingsMsg::OpenAt(
                    crate::SettingsPage::Languages,
                )))
            }
            LspMsg::Restart(language) => {
                // Drop the running server (kills its child), then re-provision.
                self.reset_lsp(&language);
                self.ensure_lsp(&language)
            }
            LspMsg::Remove { name, version } => self.on_lsp_remove(name, version),
            LspMsg::DownloadFor(language) => {
                // Force a fresh provisioning attempt for this language —
                // asked about, whatever the first open left for later; the
                // other languages stay left for later.
                self.proj.servers_asked.insert(language.clone());
                self.proj.link.lsp.remove(&language);
                self.ensure_lsp(&language)
            }
        }
    }
}

/// The PATH a test set for its thread ([`SEARCH_PATH`]) for a server's
/// own program to be looked for on, in place of the one Clew's programs
/// are given (`linerpc::child_path`: on macOS, an app opened from the
/// Finder is given the login shell's).
#[cfg(test)]
pub(crate) fn path_set_here() -> Option<std::ffi::OsString> {
    SEARCH_PATH.with(|path| path.borrow().clone())
}

/// See the test build's: none.
#[cfg(not(test))]
pub(crate) fn path_set_here() -> Option<std::ffi::OsString> {
    None
}

#[cfg(test)]
thread_local! {
    /// The PATH [`path_set_here`] gives on this thread, in place of the
    /// process's, which every test shares.
    pub(crate) static SEARCH_PATH: std::cell::RefCell<Option<std::ffi::OsString>> =
        const { std::cell::RefCell::new(None) };
}

/// The program of the name the server `provision` fetches would have, when
/// one is on `path` — found as the shell finds it, a link kept as a link:
/// a version manager's shim runs what it is named for.
pub(crate) fn on_path_for(
    provision: &LspProvision,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    lsp::store::find_in_unresolved(path?, path_name(provision)?)
}

/// The name the program the server `provision` fetches goes by on PATH —
/// where it lands in the download (`clangd_22.1.6/bin/clangd`,
/// `node_modules/.bin/pyright-langserver`) is not its name there. None for
/// a provision that fetches no program of its own.
pub(crate) fn path_name(provision: &LspProvision) -> Option<&'static str> {
    let binary = match provision {
        LspProvision::Download(d) => d.binary,
        LspProvision::Install(i) => i.binary,
        _ => return None,
    };
    std::path::Path::new(binary).file_name()?.to_str()
}

/// Whether the server's own `program`, found on PATH, is worth offering in
/// place of Clew's copy. A program on PATH can be a version manager's shim
/// for a tool that is not installed — rustup's `rust-analyzer` without the
/// component, asdf's or pyenv's for another version — there only to fail
/// when run. One that answers its version is asked it ([`version_args`]);
/// one with no way to ask (pyright's server, VS Code's) is offered as found,
/// unless it is such a shim: those are kept in a `shims` folder (asdf,
/// mise, pyenv, rbenv, nodenv), and only running it would tell.
pub(crate) fn worth_offering(program: &std::path::Path, path: &std::ffi::OsStr) -> bool {
    let name = program.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    match version_args(name) {
        Some(args) => runs_here(program, args, path),
        None => !program.components().any(|c| c.as_os_str() == "shims"),
    }
}

/// How the server program `name` is asked its version, for those that
/// answer it: `gopls version`, `--version` for the rest — and taplo whether
/// it has its language server at all (`taplo lsp --help`): npm's
/// `@taplo/cli` answers `--version` and has none. Pyright's server and VS
/// Code's (JSON, HTML, CSS) take no such flag — they refuse to start
/// without a connection — nor does intelephense or csharp-ls.
pub(crate) fn version_args(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "gopls" => Some(&["version"]),
        "taplo" => Some(&["lsp", "--help"]),
        "rust-analyzer" | "clangd" | "zls" | "haskell-language-server-wrapper" => {
            Some(&["--version"])
        }
        _ => None,
    }
}

/// Whether `program` runs here: asked with `args`, with nothing to read, it
/// exits in success within a few seconds — run as the server would be, on
/// the PATH it was found on (a `#!/usr/bin/env node` server finds its
/// `node` there: an app opened from the Finder has none on its own), and
/// away from any project.
pub(crate) fn runs_here(program: &std::path::Path, args: &[&str], path: &std::ffi::OsStr) -> bool {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let Ok(mut child) = clew_core::spawn::command(program)
        .args(args)
        .env("PATH", path)
        .current_dir(clew_core::linerpc::neutral_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}
