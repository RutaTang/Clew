//! The project watcher: debounced filesystem events refresh the shared file
//! list, the client's tree and its symbol index.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use clew_core::fs_scan::FileEntry;
use clew_protocol::{Event, ServerMessage};
use notify_debouncer_full::new_debouncer_opt;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use tokio::sync::mpsc::UnboundedSender;

use crate::index::{
    SymbolPayload, file_symbols_for, full_symbol_payload, publish_project_symbols, structure_patch,
};
use crate::transport::{OutputBudget, send_bulk};
use crate::{ProjectFiles, SharedFiles};

/// Shortest interval between two reports of watcher trouble. A backend that
/// starts failing tends to fail on every event; one line per interval says
/// the same thing without flooding the client's status bar.
const WATCH_ERROR_INTERVAL: Duration = Duration::from_secs(60);

/// Debounce window: coalesces the burst a single save or `git pull` produces.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(250);

/// The concrete debouncer type, held to keep the watch thread alive.
///
/// `NoCache`, not `RecommendedCache`. The cache's job is to stitch a rename's
/// two halves together by file id, and to do that it walks the ENTIRE root on
/// the calling thread at `watch()` time — unfiltered, `follow_links(true)`, one
/// `stat` per entry — then retains a map entry per path for the watcher's life
/// (measured at 673k entries on this repository). That cost buys nothing here:
/// the callback below never reads a stitched rename. It tests the event KIND of
/// each path that survives the noise filter, to decide `structural`, and then
/// recovers what actually changed by diffing the file set before and after a
/// rescan — which catches the vacated path and the new one whether the
/// platform reported them as one event or two.
/// Linux already built `NoCache`; this makes macOS agree.
///
/// The claim that stitching is redundant is what
/// `renaming_a_directory_updates_its_descendants_symbols` and
/// `search_sees_files_created_after_open` in `tests/protocol.rs` check; both
/// drive the watcher end to end and both pass on macOS without the cache.
pub(crate) type Watcher = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::NoCache,
>;

/// Above this many changed files in one watcher batch, republish the whole
/// project rather than patching it. A directory rename expands to every
/// descendant, and past a point the patch is both a huge frame and slower to
/// apply than a fresh snapshot.
pub(crate) const MAX_PARTIAL_FILES: usize = 400;

/// How far [`commit_open_project`] got. Three outcomes, not two: the function
/// used to answer `bool` and returned `true` both when it finished and when it
/// was superseded mid-walk, which are different states — only the first sent
/// the `Tree` reply, and the caller owes the other two an answer of its own
/// (every `OpenProject` is answered exactly once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenCommit {
    /// Superseded before anything was written. Nothing changed.
    Superseded,
    /// Files committed, then a newer open landed while we built the watcher.
    /// The watcher was dropped and NO reply was sent.
    CommittedThenSuperseded,
    /// Files committed, watcher installed, `Tree` replied.
    Replied,
}

/// Commit a finished `OpenProject` scan, install the watcher, then answer the
/// client — in that order, and with every lock dropped before the next step.
/// Returns how far it got; only [`OpenCommit::Replied`] sent the reply.
///
/// `make_watcher` is not the cheap FSEvents registration this code used to
/// assume: `notify-debouncer-full`'s file-id cache walks the entire root on
/// the calling thread — no ignore filtering, `follow_links(true)`, one `stat`
/// per entry — so on a repo carrying `target/` or `node_modules/` it is
/// seconds, and through a symlink it can leave the project altogether. It used
/// to run with the files mutex held, which put that walk in front of every
/// request parked in [`Server::wait_for_files_blocking`] and in front of the
/// previous watcher's callback. Now it runs holding nothing.
///
/// What the walk is still in front of is the reply, deliberately. The watch
/// has to be live before the client is told the project is open, or a change
/// made in that window is missed until some later structural event re-scans —
/// and the window is not theoretical: replying first was tried and measured,
/// and it loses the race widely enough that five tests in `tests/protocol.rs`
/// fail, `search_sees_files_created_after_open` on its own as well as in a
/// full run. So this does NOT shorten a project open. Only dropping the
/// file-id cache does that (`NoCache`, which is what Linux already builds),
/// and that is a change to what the watcher reports, not to this ordering.
///
/// What used to justify the single critical section — "files and watcher can
/// never disagree" — is preserved by the two epoch checks instead. The first
/// is the one that matters: with it outside the files lock a superseded open
/// could pass the check, lose the race to the newer open's commit, and then
/// overwrite it, filing project A's files under project B's root. The second
/// covers the window this split opens: while we walk, a newer open can clear
/// the watcher slot and install its own, so a stale watcher must be dropped
/// here rather than written over the live one.
///
/// `reply` and `make_watcher` are parameters so the ordering can be tested
/// without a repository big enough to make the walk observable.
///
/// [`Server::wait_for_files_blocking`]: crate::Server::wait_for_files_blocking
#[allow(clippy::too_many_arguments)] // the whole commit sequence, not state
pub(crate) fn commit_open_project(
    files_slot: &SharedFiles,
    watcher_slot: &Mutex<Option<Watcher>>,
    open_epoch: &AtomicU64,
    epoch: u64,
    root: &Path,
    files: Arc<Vec<FileEntry>>,
    reply: impl FnOnce(),
    make_watcher: impl FnOnce() -> Option<Watcher>,
) -> OpenCommit {
    {
        let mut slot = files_slot.lock().unwrap_or_else(PoisonError::into_inner);
        if open_epoch.load(Ordering::SeqCst) != epoch {
            return OpenCommit::Superseded;
        }
        *slot = Some(ProjectFiles {
            root: root.to_path_buf(),
            files,
        });
    }
    // The walk, with no lock held.
    let watcher = make_watcher();
    {
        let mut slot = watcher_slot.lock().unwrap_or_else(PoisonError::into_inner);
        if open_epoch.load(Ordering::SeqCst) != epoch {
            // Superseded while we walked. Dropping `watcher` here stops its
            // thread; installing it would leave the OLD root watched and throw
            // away the watcher the newer open already put in this slot.
            return OpenCommit::CommittedThenSuperseded;
        }
        *slot = watcher;
    }
    // Last, so that by the time the client acts on the tree the watch behind
    // it is already running.
    reply();
    OpenCommit::Replied
}

/// Watch `root` recursively; stream changes back on `out` as notifications. A
/// content change emits `FilesChanged`; a create/delete, an edit to a file that
/// defines the ignore rules, or the backend reporting that it dropped events
/// also re-scans and emits an updated `Tree`. Returns the debouncer, which must
/// be kept alive to run, or why watching is impossible.
///
/// Symlinks are NOT followed. The scanner does not follow them either, so a
/// linked directory is not part of the project; following it (the `notify`
/// default) made inotify walk and watch whatever it pointed at — `/`, `$HOME`,
/// a huge dependency cache — spending the per-user watch limit outside the
/// project and reporting changes from there.
///
/// Registration is NOT cheap: see [`commit_open_project`] for what
/// `Debouncer::watch` does to the calling thread and why nothing may wait on
/// this behind a lock.
pub(crate) fn spawn_watcher(
    root: PathBuf,
    out: UnboundedSender<ServerMessage>,
    files: SharedFiles,
    index_seq: Arc<Mutex<u64>>,
    tree_seq: Arc<AtomicU64>,
    budget: Arc<OutputBudget>,
) -> Result<Watcher, String> {
    let cb_root = root.clone();
    let mut last_error: Option<Instant> = None;
    let mut debouncer = new_debouncer_opt(
        DEBOUNCE,
        None,
        move |res: notify_debouncer_full::DebounceEventResult| match res {
            Ok(events) => on_watch_batch(
                &events, &cb_root, &out, &files, &index_seq, &tree_seq, &budget,
            ),
            // Reported, not dropped: an error here usually means changes are
            // being missed (the watch limit was hit, a watched directory went
            // away), and the reader deserves to know the tree may be stale.
            // A `Status` notice — trouble in the background, not the answer
            // to any request.
            Err(errors) => {
                if last_error.is_none_or(|at| at.elapsed() >= WATCH_ERROR_INTERVAL) {
                    last_error = Some(Instant::now());
                    let first = errors
                        .first()
                        .map_or_else(|| "unknown error".to_string(), ToString::to_string);
                    let _ = out.send(ServerMessage::Notification {
                        event: Event::Status {
                            message: format!(
                                "file watching reported a problem ({first}) — changes made \
                                 outside clew may not appear until the project is reopened"
                            ),
                        },
                    });
                }
            }
        },
        notify_debouncer_full::NoCache,
        watcher_config(),
    )
    .map_err(|e| e.to_string())?;
    debouncer
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;
    Ok(debouncer)
}

/// The watcher's backend configuration: symlinks are not followed (see
/// [`spawn_watcher`]). Its own function so the setting is pinned by a test on
/// every platform — the behavioural test of it can only fail on Linux, since
/// macOS's FSEvents never descends a symlink whatever it is told.
fn watcher_config() -> notify_debouncer_full::notify::Config {
    notify_debouncer_full::notify::Config::default().with_follow_symlinks(false)
}

/// [`spawn_watcher`], telling the client (with a `Status` notice) when the
/// project cannot be watched.
///
/// The project still opens — a tree the user can read is worth more than a
/// refusal — but it used to open silently unwatched (`.ok()?`), so edits made
/// outside clew simply never appeared and nothing said why. The usual cause is
/// the system's watch limit (inotify's `max_user_watches` on Linux).
pub(crate) fn watch_or_report(
    root: PathBuf,
    out: UnboundedSender<ServerMessage>,
    files: SharedFiles,
    index_seq: Arc<Mutex<u64>>,
    tree_seq: Arc<AtomicU64>,
    budget: Arc<OutputBudget>,
) -> Option<Watcher> {
    match spawn_watcher(
        root.clone(),
        out.clone(),
        files,
        index_seq,
        tree_seq,
        budget,
    ) {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            let _ = out.send(ServerMessage::Notification {
                event: Event::Status {
                    message: format!(
                        "file watching is unavailable for {} ({e}) — changes made outside \
                         clew will not appear until the project is reopened",
                        root.display()
                    ),
                },
            });
            None
        }
    }
}

/// One debounced batch from the watcher: work out what changed, refresh the
/// server's own view of the project, and notify the client.
///
/// Split out of the callback closure so a batch can be driven directly in a
/// test — in particular the lost-events batch below, which no test can provoke
/// from the kernel.
pub(crate) fn on_watch_batch(
    events: &[notify_debouncer_full::DebouncedEvent],
    cb_root: &Path,
    out: &UnboundedSender<ServerMessage>,
    files: &SharedFiles,
    index_seq: &Mutex<u64>,
    tree_seq: &AtomicU64,
    budget: &OutputBudget,
) {
    let mut rels: Vec<String> = Vec::new();
    let mut structural = false;
    // The backend told us it dropped events: inotify `Q_OVERFLOW`, FSEvents
    // `MUST_SCAN_SUBDIRS`. Everything below is then incomplete, so the batch is
    // answered from disk instead of from the events.
    let mut rescan = false;
    // Resolved once per batch: the root as the kernel reports it.
    let canon_root = cb_root.canonicalize().ok();
    for ev in events {
        // notify reports the loss as a synthetic `EventKind::Other` carrying
        // `Flag::Rescan` and NO paths. It matches none of the kinds below, so
        // it used to be skipped — discarding the one signal that says "what I
        // told you is incomplete", and leaving the changes lost with it
        // unlearned until some unrelated create/delete happened to force a
        // scan. Nothing here can name what was missed, so both halves of the
        // work are redone from disk: the file-set scan, and a FULL symbol
        // republish rather than a patch (a lost in-place edit changes no rel,
        // so the set diff below would not see it either).
        if ev.need_rescan() {
            structural = true;
            rescan = true;
            continue;
        }
        let relevant = matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        );
        if !relevant {
            continue;
        }
        // A rename changes the file SET as much as a create/delete
        // does — `Modify(Name)` is how the watcher reports it, and
        // treating it as a content change left the tree stale.
        let changes_set = matches!(
            ev.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(notify_debouncer_full::notify::event::ModifyKind::Name(_))
        );
        for p in &ev.paths {
            // Relativize FIRST. `is_noise` rejects a path if ANY of its
            // components is a build/VCS name, so testing the absolute
            // path made every event under a root that itself sits in one
            // — /work/node_modules/app, a checkout named `target` — look
            // like noise, and in-place edits there published nothing.
            let Some(rel) = relativize(p, cb_root, canon_root.as_deref()) else {
                continue;
            };
            // Decided BEFORE the noise filter on purpose: git's per-repo
            // exclude file lives under `.git`, which the filter drops.
            if is_ignore_rules(rel) {
                structural = true;
            }
            if is_noise(rel) {
                continue;
            }
            // Decided per path, AFTER the noise filter. Deciding it from the
            // event kind alone made `.git/index.lock` churn, every artifact a
            // build drops into `target/`, and each of this server's own
            // `.clew/` atomic writes a full rescan plus a whole-tree push.
            if changes_set {
                structural = true;
            }
            rels.push(rel.to_string_lossy().into_owned());
        }
    }
    // A create/delete — or an edit to the ignore rules — changes the
    // file set: re-scan, refresh the server's shared file list (so
    // search/docs/agent turns grep the current set, not the one from
    // OpenProject), and push a fresh tree.
    if structural {
        // Stamped as the scan starts (see `Event::Tree::seq`).
        let seq = tree_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (scan, report) = clew_core::fs_scan::scan_with_report(cb_root.to_path_buf());
        let tree_rels: Vec<String> = scan.files.iter().map(|f| f.rel.clone()).collect();
        let fresh = Arc::new(scan.files);
        let mut previous: Option<Arc<Vec<FileEntry>>> = None;
        {
            let mut slot = files.lock().unwrap_or_else(PoisonError::into_inner);
            // Only while this watcher's project is still the open one:
            // a late callback from a replaced watcher must not clobber
            // the next project's file list.
            if slot.as_ref().is_some_and(|p| p.root == cb_root) {
                previous = slot.as_ref().map(|p| p.files.clone());
                *slot = Some(ProjectFiles {
                    root: cb_root.to_path_buf(),
                    files: fresh.clone(),
                });
            }
        }
        // What the watcher NAMES is not what changed. A directory
        // event names the directory, never the files under it, so
        // publishing that rel updated nothing — the old path's
        // descendants kept their stale symbols and the new path's were
        // never read. Worse, a rename may be reported from one side
        // only (macOS gives the destination), so even expanding the
        // named directory would leave the vacated one behind.
        //
        // Diff the file sets instead: every rel that appeared has to
        // be read, every rel that vanished has to be cleared, whatever
        // the platform chose to tell us.
        if let Some(before) = &previous {
            let before_set: std::collections::HashSet<&str> =
                before.iter().map(|f| f.rel.as_str()).collect();
            let after_set: std::collections::HashSet<&str> =
                fresh.iter().map(|f| f.rel.as_str()).collect();
            rels.extend(
                before_set
                    .symmetric_difference(&after_set)
                    .map(|rel| (*rel).to_string()),
            );
        }
        send_bulk(
            out,
            budget,
            ServerMessage::Notification {
                event: Event::Tree {
                    root: cb_root.to_string_lossy().into_owned(),
                    seq,
                    tree: scan.tree,
                    files: tree_rels,
                    truncated: scan.truncated,
                    tracked_ignored: report.tracked_ignored.clone(),
                },
            },
        );
        // What the rescan left out or added, behind the tree it describes —
        // only while this watcher's project is still the open one (`previous`
        // is set exactly then): a notice carries no root, so a late callback
        // from a replaced watcher would describe the wrong project.
        if previous.is_some() {
            crate::send_scan_report(out, &report);
        }
    }
    rels.sort();
    rels.dedup();
    if rescan || rels.len() > MAX_PARTIAL_FILES {
        // A subtree rename expands to every descendant. Past a point a
        // patch is both a huge frame and slower to apply than a fresh
        // snapshot, so republish the project instead.
        //
        // A lost-events batch takes the same path for the opposite
        // reason: it names nothing at all, so a patch would carry the
        // set diff only and leave every file whose CONTENT changed
        // while the queue overflowed indexed as it was before.
        publish_project_symbols(out, budget, index_seq, cb_root, true, || {
            let all = files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .filter(|p| p.root == cb_root)
                .map(|p| p.files.clone())?;
            Some(full_symbol_payload(cb_root, &all))
        });
    } else if !rels.is_empty() {
        // Read and publish under the publication lock, so this
        // update's `seq` reflects when its files were READ. Without
        // that, a full snapshot still building elsewhere is stamped
        // later and overwrites these fresher entries with what those
        // files looked like before the change.
        publish_project_symbols(out, budget, index_seq, cb_root, false, || {
            // Per-file symbol updates for the changed set, so a remote
            // client's index stays fresh without local reads. A rel
            // that no longer resolves to an indexable file gets an
            // empty entry — "clear what you had". (This thread is the
            // watcher's own; the reads don't block the request loop.)
            let files_out: Vec<clew_protocol::FileSymbols> = rels
                .iter()
                .map(|rel| {
                    file_symbols_for(cb_root, &cb_root.join(rel), rel).unwrap_or_else(|| {
                        clew_protocol::FileSymbols {
                            rel: rel.clone(),
                            symbols: Vec::new(),
                            imports: Vec::new(),
                        }
                    })
                })
                .collect();
            // Resolution metadata and the structure index are
            // re-extracted only when their INPUTS changed, and the
            // result is sent as a `Patch` — `Set(None)` says the value
            // is GONE. Collapsing that into a bare `None` made it
            // indistinguishable from "not recomputed", so a deleted
            // `go.mod` module line kept mis-resolving every Go import
            // in the project until it was reopened.
            let go_module = match rels.iter().any(|r| r == "go.mod") {
                true => clew_protocol::Patch::Set(clew_core::imports::read_go_module(cb_root)),
                false => clew_protocol::Patch::Unchanged,
            };
            let dart_package = match rels.iter().any(|r| r == "pubspec.yaml") {
                true => clew_protocol::Patch::Set(clew_core::imports::read_dart_package(cb_root)),
                false => clew_protocol::Patch::Unchanged,
            };
            // The structure index is whole-project (a trait's
            // implementors live anywhere), so it is rebuilt rather
            // than patched. Only for batches that can affect it, on
            // the watcher's own debounced thread — never on the
            // request loop.
            let structure = if rels.iter().any(|r| r.ends_with(".rs")) {
                // Cloned out on its own line: the guard must not be
                // held across the rebuild below.
                let all = files
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map(|p| p.files.clone());
                match all {
                    Some(all) => structure_patch(cb_root, &all),
                    // Could not recompute (no project). Say nothing,
                    // rather than claim the index is gone.
                    None => clew_protocol::Patch::Unchanged,
                }
            } else {
                clew_protocol::Patch::Unchanged
            };
            Some(SymbolPayload {
                files: files_out,
                go_module,
                dart_package,
                structure,
            })
        });
    }
    // Both publication paths tell the client which files moved, so a
    // local client's own pipelines reindex the same set.
    //
    // A lost-events batch names nothing, so it sends this only for whatever the
    // set diff turned up. Residual, stated rather than papered over: an OPEN
    // buffer whose bytes changed inside the dropped burst is not re-read by the
    // client until it is touched again — the server's own index recovers above,
    // the client's editor view does not.
    if !rels.is_empty() {
        let _ = out.send(ServerMessage::Notification {
            event: Event::FilesChanged {
                root: cb_root.to_string_lossy().into_owned(),
                rels,
            },
        });
    }
}

/// `path` relative to the project root, which the platform may report either
/// as the client named it (`root`) or resolved (`canon`): FSEvents reports
/// resolved paths, so under a root reached through a symlink — the system temp
/// dir on macOS, a `~/src -> /Volumes/...` link — nothing stripped against
/// `root` alone, and every in-place edit there was silently dropped.
fn relativize<'a>(path: &'a Path, root: &Path, canon: Option<&Path>) -> Option<&'a Path> {
    path.strip_prefix(root)
        .ok()
        .or_else(|| canon.and_then(|c| path.strip_prefix(c).ok()))
}

/// Skip VCS internals, build output, dependencies, and clew's own data dir so a
/// `cargo build` or `npm install` doesn't drown the channel. `rel` is
/// root-relative.
///
/// Exactly the directories the scanner prunes ([`fs_scan::NOISE_DIRS`] —
/// nothing under them is in the file set, so nothing there can change it),
/// plus churn a reader never needs live: other VCSs' metadata, IDE workspace
/// state, Finder's `.DS_Store`. The `.clew` configurations the scanner DOES
/// list ([`fs_scan::VISIBLE_CLEW_FILES`]) are not noise, or creating one would
/// never reach the tree and editing one never its open view.
///
/// [`fs_scan::NOISE_DIRS`]: clew_core::fs_scan::NOISE_DIRS
/// [`fs_scan::VISIBLE_CLEW_FILES`]: clew_core::fs_scan::VISIBLE_CLEW_FILES
pub(crate) fn is_noise(rel: &Path) -> bool {
    if is_visible_clew_file(rel) {
        return false;
    }
    rel.components().any(|c| {
        let name = c.as_os_str();
        clew_core::fs_scan::is_noise_dir(name)
            || matches!(name.to_str(), Some(".hg" | ".svn" | ".idea" | ".DS_Store"))
    })
}

/// `.clew/lsp.toml`, `.clew/launch.json`: part of the project's file set.
fn is_visible_clew_file(rel: &Path) -> bool {
    let mut parts = rel.components().map(|c| c.as_os_str());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(dir), Some(leaf), None) => {
            dir == ".clew"
                && leaf
                    .to_str()
                    .is_some_and(|l| clew_core::fs_scan::VISIBLE_CLEW_FILES.contains(&l))
        }
        _ => false,
    }
}

/// Does this root-relative path define which files belong to the project? The
/// scanner re-reads these on every scan, so an edit to one changes the file SET
/// without creating or removing anything: a plain in-place write (`echo >>`,
/// `sed -i`) is a bare `Modify(Data)`, nothing else in the batch marks it
/// structural, and the stale rules survive — newly ignored files stay in the
/// tree and the search set, newly un-ignored ones stay invisible — until some
/// unrelated create/delete or a reopen forces a rescan. (Atomic-write editors
/// and `git checkout` emit Create/`Modify(Name)` and were always covered.)
///
/// An ignore file inside a pruned directory is not one of these: the scanner
/// never descends there, so it cannot change the file set — and they are
/// common noise (an `npm install` drops hundreds into `node_modules`, and
/// clew's own first state write creates `.clew/.gitignore`).
pub(crate) fn is_ignore_rules(rel: &Path) -> bool {
    // The repo-local exclude list, equal in force to a `.gitignore`. It lives
    // under `.git`, which is noise, hence decided first.
    if rel == Path::new(".git/info/exclude") {
        return true;
    }
    matches!(
        rel.file_name().and_then(|n| n.to_str()),
        Some(".gitignore" | ".ignore")
    ) && !rel.parent().is_some_and(is_noise)
}

#[cfg(test)]
mod tests {
    use super::{
        OpenCommit, Watcher, commit_open_project, on_watch_batch, spawn_watcher, watch_or_report,
    };
    use crate::test_support::Scratch;
    use crate::transport::OutputBudget;
    use crate::{ProjectFiles, SharedFiles};
    use clew_protocol::{Event, ServerMessage};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// The watcher never follows a symlink out of the project — pinned here,
    /// where it can fail on any platform: the behavioural test
    /// (`the_watcher_does_not_follow_a_symlink_out_of_the_project`) can only
    /// fail on Linux, as FSEvents never descends a symlink anyway.
    #[test]
    fn the_watcher_is_configured_not_to_follow_symlinks() {
        assert!(!super::watcher_config().follow_symlinks());
    }

    /// Registering the watch must hold no lock. `Debouncer::watch` is not the
    /// cheap syscall this code once assumed: it walks the whole root —
    /// unfiltered, following symlinks, one `stat` per entry — on the calling
    /// thread, seconds on a repo carrying `target/` or `node_modules/`. It ran
    /// inside the commit's critical section, so every request parked in
    /// `wait_for_files_blocking` (Search, ReadSources, AgentAsk, BuildDocs) and
    /// the previous watcher's callback waited it out.
    ///
    /// The reply stays behind it on purpose, and that half is asserted here
    /// too: the watch must be live before the client is told the project is
    /// open, or a change made in that window is missed until some later
    /// structural event re-scans. Moving the reply first does shorten the open,
    /// and it also loses that race consistently enough to fail four watcher
    /// tests in `tests/protocol.rs`.
    ///
    /// Rendezvous, not timing: the factory parks INSIDE the walk, so the
    /// assertions observe exactly the state at that instant.
    #[test]
    fn an_open_registers_its_watch_holding_no_lock_and_replies_only_after() {
        let files: SharedFiles = Arc::new(Mutex::new(None));
        let watcher: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(7));
        let (entered_walk, in_walk) = std::sync::mpsc::channel::<()>();
        let (may_finish, finish_now) = std::sync::mpsc::channel::<()>();
        let (replied, saw_reply) = std::sync::mpsc::channel::<()>();
        let dir = crate::test_support::Scratch::new("server-open-order");
        let root = dir.to_path_buf();
        let (f, w, e, r) = (files.clone(), watcher.clone(), epoch.clone(), root.clone());
        let task = std::thread::spawn(move || {
            commit_open_project(
                &f,
                &w,
                &e,
                7,
                &r,
                Arc::new(Vec::new()),
                || replied.send(()).unwrap(),
                || {
                    entered_walk.send(()).unwrap();
                    finish_now.recv().unwrap();
                    None
                },
            )
        });
        in_walk.recv().unwrap();
        // The committed file list is reachable while the walk runs, not locked
        // away for its duration...
        let guard = files
            .try_lock()
            .expect("the files lock must not be held across the watch registration");
        assert_eq!(
            guard.as_ref().map(|p| p.root.clone()),
            Some(root),
            "the file list must be committed before the walk, not after it"
        );
        drop(guard);
        assert!(
            watcher.try_lock().is_ok(),
            "the watcher lock must not be held across the registration either"
        );
        // ...and the client has not been told yet, because the watch it will
        // act against is not running.
        assert!(
            saw_reply.try_recv().is_err(),
            "the Tree reply must follow the watch registration"
        );
        may_finish.send(()).unwrap();
        assert_eq!(
            task.join().unwrap(),
            OpenCommit::Replied,
            "the commit ran to completion"
        );
        assert!(saw_reply.try_recv().is_ok(), "and the reply did go out");
    }

    /// Building the watcher outside the commit's critical section opens a
    /// window: a newer `OpenProject` can clear the slot and install its own
    /// while we walk. The stale watcher must then be DROPPED — writing it over
    /// the live one would leave the old root watched and the new project not
    /// watched at all, the disagreement the single critical section used to
    /// rule out.
    #[test]
    fn a_watcher_built_for_a_superseded_open_is_dropped_not_installed() {
        let dir = Scratch::new("open-superseded");
        let root = dir.to_path_buf();
        // `spawn_watcher` needs a channel and a sequence counter; nothing here
        // reads them, and a dropped receiver only makes the callback's sends
        // no-ops.
        let make = |root: std::path::PathBuf| {
            let (out, _rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_watcher(
                root,
                out,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(0)),
                Arc::new(AtomicU64::new(0)),
                OutputBudget::new(),
            )
            .ok()
        };

        // Control: nothing supersedes it, so the watcher is installed.
        let files: SharedFiles = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(1));
        let committed = commit_open_project(
            &files,
            &slot,
            &epoch,
            1,
            &root,
            Arc::new(Vec::new()),
            || {},
            || make(root.clone()),
        );
        assert_eq!(committed, OpenCommit::Replied);
        assert!(
            slot.lock().unwrap().is_some(),
            "an open that was not superseded installs its watcher"
        );

        // Superseded DURING the walk: the file list was still committed under
        // the matching epoch, but the watcher must not land.
        let files: SharedFiles = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(1));
        let bumping = epoch.clone();
        let committed = commit_open_project(
            &files,
            &slot,
            &epoch,
            1,
            &root,
            Arc::new(Vec::new()),
            || {},
            || {
                bumping.fetch_add(1, Ordering::SeqCst); // a newer OpenProject
                make(root.clone())
            },
        );
        assert_eq!(
            committed,
            OpenCommit::CommittedThenSuperseded,
            "the commit itself won its race, but the reply never went out"
        );
        assert!(
            slot.lock().unwrap().is_none(),
            "a watcher built for a superseded open must be dropped, not installed"
        );
    }

    /// The backend can report that it LOST events — inotify `Q_OVERFLOW`,
    /// FSEvents `MUST_SCAN_SUBDIRS` — and notify passes that on as a synthetic
    /// `EventKind::Other` carrying `Flag::Rescan` and no paths. It matched none
    /// of the kinds the callback looks for, so the one signal meaning "what I
    /// told you is incomplete" was dropped and the changes lost with it were
    /// never learned.
    ///
    /// It has to drive the whole recovery instead: re-scan the set (refreshed
    /// shared file list, fresh `Tree`) and republish EVERY file's symbols,
    /// because the batch names no file that a patch could carry — a content
    /// edit lost in the burst appears in no set diff.
    ///
    /// The kernel cannot be made to overflow from a test, so the batch is
    /// handed to `on_watch_batch` directly.
    #[test]
    fn a_lost_events_signal_drives_a_full_rescan() {
        use notify_debouncer_full::DebouncedEvent;
        use notify_debouncer_full::notify::{EventKind, event::Flag};

        let dir = Scratch::new("watch-rescan");
        let root = dir.to_path_buf();
        std::fs::write(root.join("a.rs"), "pub fn appeared() {}\n").unwrap();

        // Deliberately stale: the project is open with an EMPTY file list, so
        // nothing below can pass unless the batch itself went back to disk.
        let stale = || -> SharedFiles {
            Arc::new(Mutex::new(Some(ProjectFiles {
                root: root.clone(),
                files: Arc::new(Vec::new()),
            })))
        };
        let batch = |ev: DebouncedEvent, files: &SharedFiles| {
            let (out, rx) = tokio::sync::mpsc::unbounded_channel();
            on_watch_batch(
                &[ev],
                &root,
                &out,
                files,
                &Mutex::new(0),
                &AtomicU64::new(0),
                &OutputBudget::new(),
            );
            drop(out);
            rx
        };

        // Control first: `Other` WITHOUT the flag is an uninteresting event
        // (notify uses it for anything it can't classify) and must stay
        // ignored. If this ever fires, the gate below was widened to the kind
        // rather than to the flag.
        let files = stale();
        let mut rx = batch(
            DebouncedEvent::new(
                notify_debouncer_full::notify::Event::new(EventKind::Other),
                std::time::Instant::now(),
            ),
            &files,
        );
        assert!(
            rx.try_recv().is_err(),
            "an unflagged `Other` event must publish nothing"
        );
        assert!(
            files.lock().unwrap().as_ref().unwrap().files.is_empty(),
            "and must not re-scan the project"
        );

        // The real thing.
        let files = stale();
        let mut rx = batch(
            DebouncedEvent::new(
                notify_debouncer_full::notify::Event::new(EventKind::Other).set_flag(Flag::Rescan),
                std::time::Instant::now(),
            ),
            &files,
        );

        assert!(
            files
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.rel == "a.rs"),
            "the server's own file list must re-converge: search, docs and agent \
             turns grep this list"
        );
        let (mut tree, mut symbols) = (false, None);
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ServerMessage::Notification {
                    event: Event::Tree { files, .. },
                    ..
                } => tree = files.iter().any(|r| r == "a.rs"),
                ServerMessage::Notification {
                    event: Event::ProjectSymbols { full, files, .. },
                    ..
                } => symbols = Some((full, files)),
                _ => {}
            }
        }
        assert!(tree, "the client must be sent a rebuilt tree");
        let (full, files) = symbols.expect("the symbol index must be republished");
        assert!(
            full,
            "the republish must be FULL: a patch carries only the files the batch \
             named, and this batch names none"
        );
        assert!(
            files.iter().any(|f| f.rel == "a.rs"),
            "and it must carry the project's symbols"
        );
    }

    /// `structural` used to be decided from the event KIND before the noise
    /// filter ran, so VCS churn (`.git/index.lock`), build output (`target/`),
    /// dependency installs and this server's own `.clew/` atomic writes each
    /// forced a full rescan and a whole-tree push, every debounce. Noise must
    /// change nothing — not the file list, not the client — while a real
    /// change still rescans, and so does an in-place edit of the ignore rules
    /// (git's exclude file lives under `.git`, which the filter drops).
    #[test]
    fn noise_never_rescans_and_real_changes_still_do() {
        use notify_debouncer_full::DebouncedEvent;
        use notify_debouncer_full::notify::event::{
            CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
        };
        use notify_debouncer_full::notify::{Event as FsEvent, EventKind};

        let root = crate::test_support::Scratch::new("server-watch-noise");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
        // Deliberately stale: an EMPTY file list, so any rescan shows.
        let stale = || -> SharedFiles {
            Arc::new(Mutex::new(Some(ProjectFiles {
                root: root.to_path_buf(),
                files: Arc::new(Vec::new()),
            })))
        };
        let batch = |events: Vec<FsEvent>, files: &SharedFiles| {
            let (out, rx) = tokio::sync::mpsc::unbounded_channel();
            let events: Vec<DebouncedEvent> = events
                .into_iter()
                .map(|e| DebouncedEvent::new(e, std::time::Instant::now()))
                .collect();
            on_watch_batch(
                &events,
                &root,
                &out,
                files,
                &Mutex::new(0),
                &AtomicU64::new(0),
                &OutputBudget::new(),
            );
            drop(out);
            rx
        };
        let event = |kind: EventKind, rel: &str| FsEvent::new(kind).add_path(root.join(rel));

        let noise = [
            ".git/index.lock",
            ".git/objects/ab/cdef0123",
            "target/debug/build/app-1234/out/gen.rs",
            ".clew/.bookmarks.json.4242.0.tmp",
            ".clew/bookmarks.json",
            "node_modules/pkg/index.js",
            // Ignore files inside pruned directories cannot change the set:
            // clew's own first state write creates the first one.
            ".clew/.gitignore",
            "node_modules/pkg/.gitignore",
            ".venv/lib/site.py",
        ];
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
        ] {
            let files = stale();
            let mut rx = batch(noise.iter().map(|rel| event(kind, rel)).collect(), &files);
            assert!(
                rx.try_recv().is_err(),
                "{kind:?} on noise must publish nothing (no Tree, no symbols, no FilesChanged)"
            );
            assert!(
                files.lock().unwrap().as_ref().unwrap().files.is_empty(),
                "{kind:?} on noise must not rescan the project"
            );
        }

        // A real file appearing among the noise still rescans.
        std::fs::write(root.join("src/b.rs"), "pub fn b() {}\n").unwrap();
        let files = stale();
        let mut events: Vec<FsEvent> = noise
            .iter()
            .map(|rel| event(EventKind::Create(CreateKind::File), rel))
            .collect();
        events.push(event(EventKind::Create(CreateKind::File), "src/b.rs"));
        let mut rx = batch(events, &files);
        let mut tree = None;
        while let Ok(msg) = rx.try_recv() {
            if let ServerMessage::Notification {
                event: Event::Tree { files, .. },
                ..
            } = msg
            {
                tree = Some(files);
            }
        }
        let tree = tree.expect("a real create rescans and pushes the tree");
        assert!(tree.iter().any(|r| r == "src/b.rs"), "{tree:?}");

        // An in-place edit of git's exclude list is structural even though it
        // lives under `.git`.
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        let files = stale();
        let mut rx = batch(
            vec![event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                ".git/info/exclude",
            )],
            &files,
        );
        let rescanned = std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
            matches!(
                m,
                ServerMessage::Notification {
                    event: Event::Tree { .. },
                    ..
                }
            )
        });
        assert!(rescanned, "an ignore-rules edit changes the file set");

        // The `.clew` configurations the scanner lists are project files, not
        // noise: creating one rescans.
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(root.join(".clew/lsp.toml"), "[rust]\n").unwrap();
        let files = stale();
        let mut rx = batch(
            vec![event(EventKind::Create(CreateKind::File), ".clew/lsp.toml")],
            &files,
        );
        let listed = std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
            matches!(
                m,
                ServerMessage::Notification {
                    event: Event::Tree { ref files, .. },
                    ..
                } if files.iter().any(|f| f == ".clew/lsp.toml")
            )
        });
        assert!(listed, "a visible .clew config reaches the tree");
    }

    /// A project that cannot be watched still opens, but the client is told:
    /// it used to open silently unwatched, so outside edits never appeared and
    /// nothing said why.
    #[test]
    fn an_unwatchable_project_says_so() {
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let base = crate::test_support::Scratch::new("server-watch-missing");
        let missing = base.join("missing");
        let watcher = watch_or_report(
            missing,
            out,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(0)),
            Arc::new(AtomicU64::new(0)),
            OutputBudget::new(),
        );
        assert!(watcher.is_none());
        match rx.try_recv() {
            Ok(ServerMessage::Notification {
                event: Event::Status { message },
            }) => assert!(
                message.contains("file watching is unavailable"),
                "{message}"
            ),
            other => panic!("the failure must be reported, got {other:?}"),
        }
    }
}
