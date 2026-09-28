//! The approval gate for repository-specified language servers, shared by
//! every spawn path (`SpawnLsp`, `LspResolve`/`LspInstall`, and the Ask
//! agent's semantic tools).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clew_protocol::{ErrorCode, Event, ServerMessage};
use tokio::sync::mpsc::UnboundedSender;

/// Client-granted approvals for repo-specified language-server commands:
/// `language` → the approved fingerprint (see `trust::lsp_fingerprint`).
/// Shared with the agent's LSP pool so **every** spawn path checks the same
/// gate — the GUI's SpawnLsp and the Ask agent's semantic tools alike.
pub type SharedApprovals = Arc<Mutex<HashMap<String, String>>>;

/// Whether `fingerprint` is approved for `language` in `root`: in the set the
/// client pushed (`LspApprovals`), or in this host's own trust store (client
/// and server on one machine). The pushed set's lock is released BEFORE the
/// store is read: that read is disk I/O, and the request loop takes the same
/// lock for every `LspApprovals` and `OpenProject`, which a spawn resolving on
/// a blocking thread used to hold up for the length of it.
fn is_approved(
    approvals: &SharedApprovals,
    root: &Path,
    language: &str,
    fingerprint: &str,
) -> bool {
    is_approved_or(approvals, language, fingerprint, || {
        clew_core::trust::Trust::load().is_lsp_approved(None, root, language, fingerprint)
    })
}

/// [`is_approved`] with the trust-store lookup injected: `stored` runs only
/// when the pushed set does not hold the fingerprint, and never under the
/// pushed set's lock.
fn is_approved_or(
    approvals: &SharedApprovals,
    language: &str,
    fingerprint: &str,
    stored: impl FnOnce() -> bool,
) -> bool {
    // The guard is a temporary of this statement, dropped at its end, before
    // `stored` runs; bound to a name, it would live on across the store's
    // disk read.
    let pushed = approvals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(language)
        .is_some_and(|f| f == fingerprint);
    pushed || stored()
}

/// The one decision point for repo-specified language-server commands: may
/// this exact command run for `language` in `root`? Approved when the client
/// pushed a matching fingerprint (`LspApprovals`), or when this host's own
/// trust store records one (the local-server case, where client and server
/// share a machine). Errors name the reason — including a fingerprint that
/// can't be computed (unreadable command).
///
/// Returns the path to SPAWN: clew's own copy of the approved bytes, taken
/// from the same handle they were hashed from. The repository's path is never
/// executed — hashing a name and then spawning that name is a check-to-exec
/// race the repository wins by replacing the leaf, the symlink, or a parent
/// directory in between.
///
/// Takes the whole resolved `server` rather than its parts, because the
/// fingerprint now covers `init_options` too: those options come from the same
/// repo-shipped `lsp.toml` as the command, several servers run programs named
/// in them, and the caller hands the very same `server.init_options` to
/// `initialize`. Passing the struct is what keeps approved-and-run in step —
/// with loose fields a caller could fingerprint one set of options and send
/// another. `command` is passed alongside because the caller has already
/// established that `server.command` is `Some` (the `None` case never gets
/// here, and never asks for approval at all).
pub fn lsp_command_allowed(
    approvals: &SharedApprovals,
    root: &Path,
    server: &clew_core::lsp::config::EffectiveServer,
    command: &Path,
) -> Result<PathBuf, String> {
    let language = server.language.as_str();
    let staged = clew_core::trust::stage_lsp_command(
        root,
        command,
        &server.args,
        &server.server_name,
        &server.version,
        server.init_options.as_ref(),
        |fingerprint| is_approved(approvals, root, language, fingerprint),
    )
    .map_err(|e| format!("cannot fingerprint the {language} server command: {e}"))?;
    staged.exec_path.ok_or_else(|| {
        format!(
            "refused: this project's lsp.toml command for {language} is not approved — \
             open a {language} file in clew and approve it there"
        )
    })
}

/// The fingerprint for the options-only shape (`init_options`, no `command`).
///
/// One derivation, two callers on purpose: the gate ([`approved_init_options`])
/// decides with it, and [`resolve_lsp`] puts it in front of the user as
/// the value to approve. If those two computed it separately and ever drifted,
/// approving would record a fingerprint the gate does not recognise and the
/// modal would come straight back with no way out of the loop.
pub(crate) fn options_only_fingerprint(
    server: &clew_core::lsp::config::EffectiveServer,
    options: &serde_json::Value,
) -> Result<String, String> {
    clew_core::trust::lsp_options_fingerprint(
        &server.args,
        &server.server_name,
        &server.version,
        options,
    )
}

/// The other half of what a repo's `lsp.toml` decides: the `init_options` it
/// asks clew to put in `initialize`. Returns the options that may be sent,
/// and — when they were withheld — the reason, for the caller to report.
///
/// These need approval in their own right. A config that sets options and no
/// `command` names no bytes to hash, so it used to reach `initialize` with no
/// consent step of any kind: the binary came from the store (consented at
/// install), and the options went out verbatim. They are the same
/// attacker-chosen input as a `command` — rust-analyzer runs
/// `cargo.buildScripts.overrideCommand` on workspace load, pyright executes
/// `python.pythonPath` to enumerate `sys.path` — so cloning a repository and
/// opening one file was code execution on this host.
///
/// Withheld rather than fatal, which is where this deliberately differs from
/// [`lsp_command_allowed`]: nothing reachable from here can ask the user
/// anything (a headless backend, and the agent's pool has no UI at all), so
/// refusing to start would take the language server away for a config that may
/// be perfectly legitimate. A server running with clew's own defaults is the
/// smaller loss. The approval itself is granted in the client, and arrives
/// here as `LspApprovals` — [`resolve_lsp`] hands the client what that
/// approval needs, so a withheld config can be allowed rather than being stuck.
pub fn approved_init_options(
    approvals: &SharedApprovals,
    root: &Path,
    server: &clew_core::lsp::config::EffectiveServer,
) -> (Option<serde_json::Value>, Option<String>) {
    let Some(options) = server.init_options.clone() else {
        return (None, None); // nothing repo-controlled to approve
    };
    let language = server.language.as_str();
    // One invariant, two shapes of approval: the options in hand must be
    // covered by a fingerprint on record. With a `command` they are inside
    // that command's fingerprint; without one they are hashed alone. The
    // command case is re-derived here rather than assumed from the caller's
    // earlier `lsp_command_allowed`, so a config that changed underneath in
    // between withholds the options instead of inheriting an answer given
    // about different ones.
    let fingerprint = match &server.command {
        Some(command) => clew_core::trust::lsp_fingerprint(
            root,
            command,
            &server.args,
            &server.server_name,
            &server.version,
            Some(&options),
        ),
        None => options_only_fingerprint(server, &options),
    };
    let fingerprint = match fingerprint {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            return (
                None,
                Some(format!(
                    "this project's lsp.toml init_options for {language} cannot be \
                     fingerprinted ({e}) — they were not sent to the server"
                )),
            );
        }
    };
    // Same two sources as the command gate (`is_approved`).
    if is_approved(approvals, root, language, &fingerprint) {
        return (Some(options), None);
    }
    (
        None,
        Some(format!(
            "this project's lsp.toml init_options for {language} are not approved — \
             they were NOT sent to the language server"
        )),
    )
}

/// The consent prompt for what installing `server` would do — or `None` when
/// `located` installs nothing.
///
/// The ONE place the offer is made: [`resolve_lsp`] sends it, and a consent
/// comes back as its `consent` — the resolution's
/// [`install_digest`](clew_core::lsp::store::Located::install_digest), which
/// covers everything the install would fetch or run and where it lands.
/// [`install_lsp`] runs only a resolution that still has that digest.
fn install_offer(
    server: &clew_core::lsp::config::EffectiveServer,
    located: &clew_core::lsp::store::Located,
) -> Option<clew_protocol::LspResolution> {
    use clew_core::lsp::store::Located;
    let describe = match located {
        Located::NeedsDownload { download, .. } => format!("download {}", download.url),
        Located::NeedsInstall { install, .. } => {
            format!("{} (requires {} on PATH)", install.describe, install.tool)
        }
        Located::Ready(_) | Located::RepoCommand(_) | Located::Unsupported(_) => return None,
    };
    Some(clew_protocol::LspResolution::NeedsInstall {
        server: server.server_name.clone(),
        version: server.version.clone(),
        describe,
        consent: located.install_digest()?.to_hex(),
    })
}

/// Why a `SpawnLsp` will not run: the `Error` to reply with. Every refusal
/// is answered, as a failed `SpawnProcess` is — "no server is configured"
/// included, which used to end the proxy (EOF) with no reply at all — besides
/// the `ProcessExited` that ends the proxy.
pub(crate) type SpawnRefusal = (ErrorCode, String);

/// Resolve what `SpawnLsp` must execute for `language`, running the
/// approval gate for repo-specified commands (blocking — it hashes the
/// executable). `Err((code, message))` is the `Error` the client is told why
/// with.
pub(crate) fn resolve_spawn_exe(
    approvals: &SharedApprovals,
    root: &Path,
    language: &str,
) -> Result<(PathBuf, Vec<String>), SpawnRefusal> {
    // A config that fails to load is an ERROR, not "use defaults": the
    // default could resolve (and run) a different server than the one
    // the project configured, silently.
    let config =
        clew_core::lsp::config::ProjectLspConfig::load(root).map_err(|e| (ErrorCode::Failed, e))?;
    let Some(server) = config.resolve(language) else {
        return Err((
            ErrorCode::Refused,
            format!("no {language} language server is configured for this project"),
        ));
    };
    use clew_core::lsp::store::Located;
    let exe = match server.command.clone() {
        // A `command` comes from the project's own lsp.toml, which ships
        // with the repository. Run it only through the one shared gate
        // every spawn path uses.
        // The approved bytes, copied where the repository cannot reach
        // them. Never the repository's own path.
        Some(cmd) => lsp_command_allowed(approvals, root, &server, &cmd)
            .map_err(|e| (ErrorCode::Refused, e))?,
        // No `command`: the store-installed binary, whose consent was the
        // install. This path ships no `init_options` — the client runs the
        // handshake over the proxied stdio, so the options it sends are
        // the ones `resolve_lsp` handed it, and that is where they are
        // gated ([`approved_init_options`]).
        None => match clew_core::lsp::store::locate(&server) {
            Located::Ready(exe) => exe,
            // Not installed on this host. Spawning must never install:
            // consent lives in the client, and it arrives as an explicit
            // `LspInstall` — a client that skipped that step gets an
            // error, not a download.
            Located::NeedsDownload { .. } | Located::NeedsInstall { .. } => {
                return Err((
                    ErrorCode::Refused,
                    format!(
                        "the {language} server is not installed on this host — \
                         it must be installed (with the user's consent) first"
                    ),
                ));
            }
            // Handled by the `Some(cmd)` arm above; never spawned as-is.
            Located::RepoCommand(_) => {
                return Err((
                    ErrorCode::Refused,
                    "a repo-specified command must go through its approval".into(),
                ));
            }
            Located::Unsupported(message) => return Err((ErrorCode::Failed, message)),
        },
    };
    Ok((exe, server.args))
}

/// What stands between the client and a running `language` server on this
/// host — the read-only resolution behind `LspResolve` (and the state
/// reported back after an `LspInstall`). Touches nothing: no downloads,
/// no spawns.
///
/// This is also the gate for the repo's `init_options` on the remote path.
/// The client runs the LSP handshake itself over the proxied stdio, so
/// whatever leaves here in `init_options` is exactly what reaches
/// `initialize`, and the client cannot re-derive the verdict itself: the
/// fingerprint covers THIS host's server/version/args, which the client
/// never sees. Unapproved options therefore never leave in `init_options`,
/// and the user is told so through `out`.
///
/// They do leave in `Ready::withheld`, which is the grant path rather than
/// a hole in the gate: it carries the fingerprint and the options only so
/// the client can SHOW them and record an approval against them. Nothing
/// runs on that copy — an allow re-enters here, and the options that reach
/// `initialize` are the ones re-read and re-fingerprinted on this host.
pub(crate) fn resolve_lsp(
    out: &UnboundedSender<ServerMessage>,
    approvals: &SharedApprovals,
    root: &Path,
    language: &str,
) -> clew_protocol::LspResolution {
    use clew_core::lsp::store::Located;
    use clew_protocol::LspResolution;
    // Surface a broken config instead of silently resolving defaults.
    let config = match clew_core::lsp::config::ProjectLspConfig::load(root) {
        Ok(config) => config,
        Err(message) => return LspResolution::Unsupported { message },
    };
    let Some(server) = config.resolve(language) else {
        return LspResolution::Unsupported {
            message: format!("no language server is configured for {language}"),
        };
    };
    // A `command` config carries its options inside the command's
    // fingerprint, and the client uses them only after approving it — so
    // this branch is unchanged, and the options-only gate below would only
    // duplicate the approval the modal is already asking for.
    if let Some(cmd) = server.command.clone() {
        return match clew_core::trust::lsp_fingerprint(
            root,
            &cmd,
            &server.args,
            &server.server_name,
            &server.version,
            server.init_options.as_ref(),
        ) {
            Ok(fingerprint) => LspResolution::Command(clew_protocol::LspCommandSpec {
                command: cmd.to_string_lossy().into_owned(),
                args: server.args.clone(),
                server: server.server_name.clone(),
                version: server.version.clone(),
                fingerprint,
                // Sent to the client, which runs the LSP handshake itself over
                // the proxied stdio — the options that end up in `initialize`,
                // the very value the fingerprint above was taken over.
                init_options: server.init_options.clone(),
            }),
            // Unfingerprintable (missing, not a regular file, oversized):
            // it can be neither approved nor run.
            Err(e) => LspResolution::Unsupported {
                message: format!("lsp.toml command: {e}"),
            },
        };
    }
    let located = clew_core::lsp::store::locate(&server);
    if let Some(offer) = install_offer(&server, &located) {
        return offer;
    }
    match located {
        // The store binary was consented to at install; its `init_options`
        // were not, and there is no command to fold them into — so they go
        // only if approved on their own fingerprint. Withheld ones are
        // reported rather than dropped in silence: the difference between
        // "my lsp.toml is ignored" and "my lsp.toml is broken" is the
        // whole of the user's next hour. (A `Status` notice lands in the
        // client's status bar.)
        Located::Ready(_) => {
            let (allowed, refused) = approved_init_options(approvals, root, &server);
            let was_refused = refused.is_some();
            if let Some(message) = refused {
                let _ = out.send(ServerMessage::Notification {
                    event: Event::Status { message },
                });
            }
            // A refusal is only half an answer without the means to grant
            // it: the client cannot compute this fingerprint (it covers
            // THIS host's server/version/args) and cannot read this host's
            // lsp.toml, so a withheld config that named no `command` had no
            // modal, no button and no way through — on every open, across
            // restarts and reconnects. `command: None` is the shape the
            // local path already raises for exactly this config.
            //
            // Nothing offered when the fingerprint itself failed: an
            // unfingerprintable config also refuses, and there is nothing
            // to approve there — the user would be asked to allow a value
            // that can never match.
            let withheld = server
                .init_options
                .as_ref()
                .filter(|_| was_refused)
                .and_then(|options| {
                    let fingerprint = options_only_fingerprint(&server, options).ok()?;
                    Some(clew_protocol::LspOptionsSpec {
                        server: server.server_name.clone(),
                        version: server.version.clone(),
                        args: server.args.clone(),
                        fingerprint,
                        options: options.clone(),
                    })
                });
            LspResolution::Ready {
                init_options: allowed,
                withheld,
            }
        }
        // Offered just above; kept total rather than unreachable.
        Located::NeedsDownload { .. } | Located::NeedsInstall { .. } => {
            LspResolution::Unsupported {
                message: format!("the {language} server is not installed"),
            }
        }
        // A `command` config returned `LspResolution::Command` above.
        Located::RepoCommand(_) => LspResolution::Unsupported {
            message: format!("the {language} server is repo-specified"),
        },
        Located::Unsupported(message) => LspResolution::Unsupported { message },
    }
}

/// Install the store-managed server for `language` (blocking). Only ever
/// called from the `LspInstall` request — the one path that carries the
/// user's consent — and only for the install that consent was given for.
/// Returns the post-install resolution.
///
/// `consent` is the digest of the offer the user was shown. The host is
/// resolved again here and may answer differently by now: its `lsp.toml`
/// edited while the prompt sat open (another `version`, another server),
/// which would have installed something nobody saw. Then nothing runs: the
/// user is told, and the CURRENT offer goes back, which puts the prompt in
/// front of them again for what would actually be installed.
///
/// `stop` ends the install where it stands — the download between chunks,
/// the toolchain command with everything it started — once the connection
/// leaves the project (or closes); it would otherwise run on for up to the
/// toolchain's deadline for a project nobody has open.
pub(crate) fn install_lsp(
    out: &UnboundedSender<ServerMessage>,
    approvals: &SharedApprovals,
    root: &Path,
    language: &str,
    consent: &str,
    stop: &std::sync::atomic::AtomicBool,
) -> clew_protocol::LspResolution {
    use clew_core::lsp::store::Located;
    use clew_protocol::LspResolution;
    // Surface a broken config instead of installing the default server
    // the project may have overridden or disabled.
    let config = match clew_core::lsp::config::ProjectLspConfig::load(root) {
        Ok(config) => config,
        Err(message) => return LspResolution::Unsupported { message },
    };
    let Some(server) = config.resolve(language) else {
        return LspResolution::Unsupported {
            message: format!("no language server is configured for {language}"),
        };
    };
    if server.command.is_some() {
        // A repo-specified command is approved, not installed; a client
        // sending LspInstall for it is confused — refuse.
        return LspResolution::Unsupported {
            message: format!("the {language} server is repo-specified, nothing to install"),
        };
    }
    let located = clew_core::lsp::store::locate(&server);
    if let Err(refusal) = located.check_install_consent(consent) {
        return match install_offer(&server, &located) {
            // An install other than the one approved: nothing runs, and the
            // prompt comes back for the install as it stands now.
            Some(offer) => {
                let _ = out.send(ServerMessage::Notification {
                    event: Event::Status {
                        message: format!("nothing was installed for {language}: {refusal}"),
                    },
                });
                offer
            }
            // Nothing to install at all; say why.
            None => LspResolution::Unsupported {
                message: match located {
                    Located::Unsupported(message) => message,
                    _ => refusal,
                },
            },
        };
    }
    let installed = match located {
        // Installed meanwhile (another window, a retry): nothing to do.
        Located::Ready(_) => Ok(()),
        Located::NeedsDownload { download, dest_dir } => {
            clew_core::lsp::store::download_and_install_cancellable(&download, &dest_dir, stop)
                .map(|_| ())
        }
        Located::NeedsInstall { install, dest_dir } => {
            clew_core::lsp::store::toolchain_install_cancellable(
                &install,
                &server.version,
                &dest_dir,
                stop,
            )
            .map(|_| ())
        }
        // Refused by the consent check above: nothing installs these.
        Located::RepoCommand(_) => Err("repo-specified, nothing to install".into()),
        Located::Unsupported(message) => Err(message),
    };
    match installed {
        Ok(()) => resolve_lsp(out, approvals, root, language),
        Err(e) => LspResolution::Unsupported {
            message: format!("install {language} server: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{SharedApprovals, approved_init_options, install_lsp, install_offer, resolve_lsp};
    use crate::test_support::{Scratch, alive, fake_slow_go, in_child, run_in_child, started_pid};
    use clew_protocol::{Event, LspResolution, ServerMessage};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// The pushed-approval lookup tolerates a lock another thread's panic
    /// poisoned (the request loop takes the same lock). That the lock is
    /// released before the trust store is read is
    /// `the_trust_store_is_read_without_the_approvals_lock`.
    #[test]
    fn the_approval_check_survives_a_poisoned_lock() {
        let approvals: SharedApprovals = Default::default();
        approvals.lock().unwrap().insert("rust".into(), "fp".into());
        let poisoner = approvals.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("a worker panicked holding the approvals");
        })
        .join();
        assert!(approvals.is_poisoned());
        assert!(super::is_approved(
            &approvals,
            Path::new("/p"),
            "rust",
            "fp"
        ));
    }

    /// The trust store is read with the pushed set's lock released: that read
    /// is disk I/O, and the request loop takes the same lock for every
    /// `LspApprovals` and `OpenProject` — a spawn resolving on a blocking
    /// thread held it up for the length of the read.
    #[test]
    fn the_trust_store_is_read_without_the_approvals_lock() {
        let approvals: SharedApprovals = Default::default();
        approvals
            .lock()
            .unwrap()
            .insert("rust".into(), "other".into());
        let mut read = false;
        let approved = super::is_approved_or(&approvals, "rust", "fp", || {
            read = true;
            assert!(
                approvals.try_lock().is_ok(),
                "the trust store was read under the approvals lock"
            );
            true
        });
        assert!(
            approved && read,
            "the store answers what the pushed set does not"
        );
        // A pushed match never reads the store.
        let approved = super::is_approved_or(&approvals, "rust", "other", || {
            panic!("the store was read for a pushed approval")
        });
        assert!(approved);
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// The data directory a child-side test was given (see `run_in_child`,
    /// which is how these tests get one without `set_var`).
    fn data_dir() -> PathBuf {
        std::env::var_os("CLEW_DATA_DIR")
            .map(PathBuf::from)
            .expect("the parent test sets CLEW_DATA_DIR for its child")
    }

    /// Run the child-side test `name` with a fresh, empty data directory, so
    /// nothing here reads the developer's real store or approvals.
    fn with_fresh_data_dir(name: &str) {
        let data = Scratch::new("lsp-gate-data");
        run_in_child(name, &[("CLEW_DATA_DIR", &data)]);
    }

    /// The remote twin of the client's gate. `resolve_lsp` is what feeds the
    /// client's `initialize` for a remote project — the client cannot re-derive
    /// the verdict (the fingerprint covers THIS host's server/version/args), so
    /// unapproved options must never leave in `init_options`. Left open, this
    /// was the unguarded sibling of the local path: clone a repo on the SSH
    /// host, open one file, and its `init_options` reached the language server
    /// with nothing asked.
    ///
    /// The other half is that a refusal must be grantable. `Ready::withheld`
    /// carries what the approval needs, and this pins the two halves against
    /// each other: the fingerprint the client is offered is the same one the
    /// gate then accepts. If they drifted, approving would record a value the
    /// gate does not recognise and the modal would come straight back, with no
    /// way out but closing the project.
    #[test]
    fn a_remote_resolve_withholds_init_options_until_they_are_approved() {
        with_fresh_data_dir("lsp_gate::tests::child_withholds_init_options_until_approved");
    }

    #[test]
    #[ignore = "runs in a child process, from a_remote_resolve_withholds_init_options_until_they_are_approved"]
    fn child_withholds_init_options_until_approved() {
        if !in_child() {
            return;
        }
        let data = data_dir();
        // A store-installed rust-analyzer, as any earlier project leaves.
        let version = clew_core::lsp::registry::by_name("rust-analyzer")
            .unwrap()
            .version;
        let store = data.join("servers").join("rust-analyzer").join(version);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("rust-analyzer"), b"#!/bin/sh\nexit 0\n").unwrap();

        let root = data.join("proj");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            root.join(".clew/lsp.toml"),
            "[rust.init_options]\n\
             \"rust-analyzer.cargo.buildScripts.overrideCommand\" = [\"/bin/sh\", \"-c\", \"id\"]\n",
        )
        .unwrap();

        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approvals: SharedApprovals = Arc::new(Mutex::new(HashMap::new()));
        let resolution = resolve_lsp(&out, &approvals, &root, "rust");
        let offered = match resolution {
            LspResolution::Ready {
                init_options,
                withheld,
            } => {
                assert!(
                    init_options.is_none(),
                    "unapproved options must not cross the wire, got {init_options:?}"
                );
                withheld.expect("a withheld config must come with the means to allow it")
            }
            other => panic!("expected Ready, got {other:?}"),
        };
        // What the modal will show, so the user approves what they read —
        // the options themselves, as a value the client need not re-parse.
        assert_eq!(
            offered.options["rust-analyzer.cargo.buildScripts.overrideCommand"],
            serde_json::json!(["/bin/sh", "-c", "id"]),
            "the withheld options must be shown: {:?}",
            offered.options
        );
        assert_eq!(offered.server, "rust-analyzer");
        // …and the user is told, rather than left wondering why the config
        // they committed has no effect.
        let told = std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
            matches!(m, ServerMessage::Notification { event: Event::Status { message } }
                if message.contains("init_options") && message.contains("not approved"))
        });
        assert!(told, "withholding must be reported, not silent");

        // The client approves it there and pushes the fingerprint here.
        // It pushes back exactly what it was OFFERED — the round trip the
        // grant path is made of — so the gate must accept that value.
        let config = clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap();
        let server = config.resolve("rust").unwrap();
        let fingerprint = clew_core::trust::lsp_options_fingerprint(
            &server.args,
            &server.server_name,
            &server.version,
            server.init_options.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            offered.fingerprint, fingerprint,
            "the value offered for approval must be the one the gate checks"
        );
        approvals
            .lock()
            .unwrap()
            .insert("rust".into(), offered.fingerprint.clone());
        match resolve_lsp(&out, &approvals, &root, "rust") {
            LspResolution::Ready {
                init_options,
                withheld,
            } => {
                assert_eq!(
                    init_options, server.init_options,
                    "an approved config must get its options, exactly as fingerprinted"
                );
                assert!(
                    withheld.is_none(),
                    "nothing is withheld once it is approved"
                );
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        // A commit that edits only the options loses that approval — the
        // stale fingerprint must not keep covering the new ones.
        std::fs::write(
            root.join(".clew/lsp.toml"),
            "[rust.init_options]\n\"rust-analyzer.procMacro.server\" = \"./payload\"\n",
        )
        .unwrap();
        match resolve_lsp(&out, &approvals, &root, "rust") {
            LspResolution::Ready {
                init_options,
                withheld,
            } => {
                assert!(init_options.is_none(), "edited options need a fresh answer");
                // And the fresh answer is askable: a stale approval must
                // not leave the new options unallowable either.
                let offered = withheld.expect("edited options must be offered for approval");
                assert_ne!(offered.fingerprint, fingerprint);
                assert!(
                    offered
                        .options
                        .get("rust-analyzer.procMacro.server")
                        .is_some()
                );
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        // A config with no options at all is untouched by the gate: it is
        // not withheld, so it must not raise a prompt either.
        std::fs::write(root.join(".clew/lsp.toml"), "[rust]\nenabled = true\n").unwrap();
        let quiet = resolve_lsp(&out, &approvals, &root, "rust");
        assert!(matches!(
            quiet,
            LspResolution::Ready {
                init_options: None,
                withheld: None
            }
        ));

        // And the helper agrees with itself: same inputs, same verdict,
        // whichever spawn path asks. The agent's LSP pool calls it
        // directly — an Ask turn must not be the way around the gate.
        let config = clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap();
        let server = config.resolve("rust").unwrap();
        assert_eq!(
            approved_init_options(&approvals, &root, &server).0,
            None,
            "no options in the config means nothing to send"
        );
        std::fs::write(
            root.join(".clew/lsp.toml"),
            "[rust.init_options]\n\"rust-analyzer.procMacro.server\" = \"./payload\"\n",
        )
        .unwrap();
        let config = clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap();
        let server = config.resolve("rust").unwrap();
        let (sent, withheld) = approved_init_options(&approvals, &root, &server);
        assert_eq!(sent, None, "the agent pool must withhold them too");
        assert!(withheld.is_some(), "and say why");
    }

    /// A server configured the way the tests below need it.
    fn effective(version: &str) -> clew_core::lsp::config::EffectiveServer {
        clew_core::lsp::config::EffectiveServer {
            language: "go".into(),
            server_name: "gopls".into(),
            version: version.into(),
            args: Vec::new(),
            command: None,
            init_options: None,
        }
    }

    /// The `consent` of a `NeedsInstall`; panics on anything else.
    fn consent_of(resolution: LspResolution) -> String {
        match resolution {
            LspResolution::NeedsInstall { consent, .. } => consent,
            other => panic!("expected NeedsInstall, got {other:?}"),
        }
    }

    /// The offer names the install and carries its digest — the value a
    /// consent must hand back — and an install that differs in anything that
    /// decides what runs (here the version) carries another. Resolutions that
    /// install nothing offer nothing.
    #[test]
    fn an_install_offer_carries_the_digest_of_exactly_that_install() {
        use clew_core::lsp::registry::{Install, Installer};
        use clew_core::lsp::store::Located;
        let pending = |version: &str| Located::NeedsInstall {
            install: Install {
                tool: "go",
                kind: Installer::Go {
                    module: "golang.org/x/tools/gopls",
                },
                binary: "gopls",
                version: version.into(),
                describe: format!("go install golang.org/x/tools/gopls@{version}"),
            },
            dest_dir: PathBuf::from("/store/gopls").join(version),
        };
        let located = pending("v0.18.1");
        match install_offer(&effective("v0.18.1"), &located) {
            Some(LspResolution::NeedsInstall {
                server,
                version,
                describe,
                consent,
            }) => {
                assert_eq!((server.as_str(), version.as_str()), ("gopls", "v0.18.1"));
                assert_eq!(
                    describe,
                    "go install golang.org/x/tools/gopls@v0.18.1 (requires go on PATH)"
                );
                assert_eq!(consent, located.install_digest().unwrap().to_hex());
                assert!(located.check_install_consent(&consent).is_ok());
                // Another version is another install: this consent does not
                // cover it.
                assert!(pending("v0.19.0").check_install_consent(&consent).is_err());
            }
            other => panic!("expected NeedsInstall, got {other:?}"),
        }
        for settled in [
            Located::Ready(PathBuf::from("/store/gopls/gopls")),
            Located::RepoCommand(PathBuf::from("./gopls")),
            Located::Unsupported("no".into()),
        ] {
            assert!(install_offer(&effective("v0.18.1"), &settled).is_none());
        }
    }

    /// The consent is bound to what the user was shown. An `LspInstall` whose
    /// install no longer matches what the host would do now — its `lsp.toml`
    /// pinned another version while the prompt sat open — installs nothing,
    /// says so, and answers with the CURRENT offer, so the prompt comes back
    /// for what would really run. A malformed consent is refused too.
    #[test]
    fn an_install_runs_only_what_was_approved() {
        with_fresh_data_dir("lsp_gate::tests::child_install_runs_only_what_was_approved");
    }

    #[test]
    #[ignore = "runs in a child process, from an_install_runs_only_what_was_approved"]
    fn child_install_runs_only_what_was_approved() {
        if !in_child() {
            return;
        }
        let data = data_dir();
        let root = data.join("proj");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approvals: SharedApprovals = Arc::new(Mutex::new(HashMap::new()));
        let stop = AtomicBool::new(false);
        // gopls installs through the Go toolchain, at whatever version the
        // project pins — so an edited pin changes what would run, which is
        // exactly the case to refuse. (Resolving needs no `go` on PATH; only
        // an install would, and none may happen here.)
        let shown = consent_of(resolve_lsp(&out, &approvals, &root, "go"));

        // While the prompt sat open, the repository pinned another version.
        std::fs::write(
            root.join(".clew/lsp.toml"),
            "[go]\nversion = \"v0.0.1-not-the-approved-one\"\n",
        )
        .unwrap();
        while rx.try_recv().is_ok() {}
        match install_lsp(&out, &approvals, &root, "go", &shown, &stop) {
            LspResolution::NeedsInstall {
                version, consent, ..
            } => {
                assert_eq!(version, "v0.0.1-not-the-approved-one", "the CURRENT offer");
                assert_ne!(consent, shown, "…with the digest of what would run now");
            }
            other => panic!("an unapproved install must not run, got {other:?}"),
        }
        let told = std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
            matches!(m, ServerMessage::Notification { event: Event::Status { message } }
                if message.contains("nothing was installed") && message.contains("changed"))
        });
        assert!(told, "a refused install must say why");

        // A consent that is not a digest at all is refused the same way.
        std::fs::remove_file(root.join(".clew/lsp.toml")).unwrap();
        match install_lsp(&out, &approvals, &root, "go", "not-a-digest", &stop) {
            LspResolution::NeedsInstall { consent, .. } => {
                assert_eq!(consent, shown, "the current offer comes back");
            }
            other => panic!("a malformed consent must not install, got {other:?}"),
        }
        assert!(
            !has_entries(&data.join("servers")),
            "nothing may be installed for an install nobody approved"
        );
    }

    /// An install stops when its flag is set — the connection leaving the
    /// project, or closing — together with the toolchain command it started,
    /// instead of running on (up to the toolchain's deadline) for a project
    /// nobody has open. The `go` on PATH here only says it started and waits.
    #[test]
    fn an_install_stops_when_its_flag_is_set() {
        let data = Scratch::new("lsp-gate-install-stop");
        let bin = Scratch::new("lsp-gate-slow-go");
        fake_slow_go(&bin);
        run_in_child(
            "lsp_gate::tests::child_install_stops_when_its_flag_is_set",
            &[("CLEW_DATA_DIR", &data), ("PATH", &bin)],
        );
    }

    #[test]
    #[ignore = "runs in a child process, from an_install_stops_when_its_flag_is_set"]
    fn child_install_stops_when_its_flag_is_set() {
        if !in_child() {
            return;
        }
        let data = data_dir();
        let bin = PathBuf::from(std::env::var_os("PATH").unwrap());
        let root = data.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let (out, _rx) = tokio::sync::mpsc::unbounded_channel();
        let approvals: SharedApprovals = Arc::new(Mutex::new(HashMap::new()));
        let consent = consent_of(resolve_lsp(&out, &approvals, &root, "go"));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let install =
            std::thread::spawn(move || install_lsp(&out, &approvals, &root, "go", &consent, &flag));
        let pid = started_pid(&bin);
        let stopped = std::time::Instant::now();
        stop.store(true, Ordering::Relaxed);
        let resolution = install.join().unwrap();
        assert!(
            stopped.elapsed() < std::time::Duration::from_secs(5),
            "the install must stop at once, took {:?}",
            stopped.elapsed()
        );
        assert!(
            matches!(resolution, LspResolution::Unsupported { ref message } if message.contains("cancelled")),
            "a stopped install ends as not installed, got {resolution:?}"
        );
        assert!(!alive(pid), "the toolchain command must be stopped with it");
        assert!(!has_entries(&data.join("servers").join("gopls")));
    }

    /// Whether `dir` exists and holds anything.
    fn has_entries(dir: &Path) -> bool {
        std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
    }
}
