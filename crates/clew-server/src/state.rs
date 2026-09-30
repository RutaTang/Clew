//! Project state under `<root>/.clew/`: the ordered worker that applies
//! reads, wholesale writes and entry-level merges in request order.
//!
//! **Missing is not refused.** A file that is not there reads as `None`
//! (start from nothing); a file that IS there but cannot be read safely — a
//! symlink, a FIFO, oversized, not UTF-8, unreadable — or that this build
//! cannot parse is reported as an error and never written over. Treating the
//! second as the first is how one bookmark toggle used to replace a whole
//! store with a one-entry file.

use std::path::{Path, PathBuf};

use clew_protocol::{Event, ServerMessage};
use tokio::sync::mpsc::UnboundedSender;

use crate::{Server, failed, refused};

/// One queued `.clew/` state operation. All state ops run on ONE ordered
/// worker so a read after a write — and two rapid writes of the same file —
/// apply in request order, while the (blocking) filesystem work stays off the
/// request loop.
pub(crate) struct StateJob {
    pub(crate) root: PathBuf,
    pub(crate) rel: String,
    pub(crate) id: clew_protocol::RequestId,
    pub(crate) work: StateWork,
}

pub(crate) enum StateWork {
    /// Read, replied as `StateContent` (or `Error` when refused).
    Read,
    /// Replace the file wholesale, or delete it (`None`).
    Write(Option<String>),
    /// Read-modify-write ONE entry, replied as `StateEdited` with the merged
    /// file. The worker's ordering is what makes this atomic against the other
    /// requests of this connection; against a SECOND clew-server on the same
    /// host the file lock inside the merge is (see `merge_or_why`). Applied at
    /// most once per `edit_id`, across connections and server processes: a
    /// client replays an edit whose reply its dead transport lost.
    Merge {
        merge: clew_protocol::StateMerge,
        edit_id: String,
    },
}

/// [`merge_or_why`] with the reason alone.
#[cfg(test)]
pub(crate) fn run_merge(
    path: &Path,
    rel: &str,
    merge: &clew_protocol::StateMerge,
    edit_id: &str,
) -> Result<Option<String>, String> {
    merge_or_why(path, rel, merge, edit_id).map_err(|(_, message)| message)
}

/// Apply one [`clew_protocol::StateMerge`] to `path`, returning the merged
/// file's text (`None` = the store emptied and the file was deleted).
///
/// The read and the write are one operation here, which is the whole point of
/// moving the merge server-side: the state worker is ordered, so nothing else
/// on THIS connection interleaves, and the file lock covers the case ordering
/// cannot — a second clew-server process on the same host, which is what two
/// windows of one clew produce (each window opens its own SSH session).
///
/// The one implementation is `statefile::merge_file`: it refuses — leaving
/// the file byte-for-byte as it was — when the current content was refused or
/// is not a store this build understands, and when the lock cannot be taken
/// for any reason other than a filesystem without locking. It also applies
/// each `edit_id` at most once: a replay is answered with the file as it is.
///
/// A failure says whether it is the store refusing the edit (`true`: its
/// content cannot be read safely or understood — permanent until the file
/// changes) or the attempt failing (an I/O error, a lock: worth sending
/// again). The reply says which ([`ErrorCode::Refused`] or
/// [`ErrorCode::Failed`]): a client drops a refused edit, and keeps a failed
/// one to try again.
///
/// [`ErrorCode::Refused`]: clew_protocol::ErrorCode::Refused
/// [`ErrorCode::Failed`]: clew_protocol::ErrorCode::Failed
fn merge_or_why(
    path: &Path,
    rel: &str,
    merge: &clew_protocol::StateMerge,
    edit_id: &str,
) -> Result<Option<String>, (bool, String)> {
    clew_core::statefile::merge_file(path, merge, edit_id)
        .map(|merged| merged.text)
        .map_err(|e| {
            (
                clew_core::statefile::is_refusal(&e),
                format!("edit .clew/{rel}: {e}"),
            )
        })
}

/// Read one state file for the client: `Ok(None)` when it does not exist, an
/// error when it exists but was refused.
///
/// An error rather than "missing" on purpose. The client holds its writes for
/// a file until the read of it lands, precisely so it never pushes an empty
/// baseline over content it has not seen; answering "missing" for a refused
/// file released those writes over it.
fn run_read(path: &Path, rel: &str) -> Result<Option<String>, String> {
    clew_core::statefile::read_checked(path)
        .map_err(|e| format!("cannot read .clew/{rel}: {e} — it was left untouched"))
}

/// Whether the file at `path` may be replaced wholesale by `incoming` (`None`
/// deletes it).
///
/// A wholesale write is last-writer-wins by design — it is only used for the
/// stores one client owns outright (`history.json`, `reading.toml`) — but only
/// among files this build understands. Refused, and left alone:
///
/// - a file that exists but cannot be read safely (see [`run_read`]);
/// - a `.json` or `.toml` file that does not parse: a hand edit with a typo, a
///   merge conflict — overwriting it throws away what the user would repair;
/// - a file stamped with a newer `schema_version` than the one being written
///   (absent counts as 1): it came from a newer clew, and this client cannot
///   know what it would lose. A delete counts as writing schema 1.
///
/// The server cannot know each store's full schema — those live in the client
/// — so beyond parsing, the writer's own stamp is the yardstick.
pub(crate) fn check_replaceable(
    path: &Path,
    rel: &str,
    incoming: Option<&str>,
) -> Result<(), String> {
    let refuse = |why: String| {
        Err(format!(
            "refused to overwrite .clew/{rel}: {why} — it was left untouched"
        ))
    };
    let existing = match clew_core::statefile::read_checked(path) {
        Ok(None) => return Ok(()),
        Ok(Some(text)) => text,
        Err(e) => return refuse(e.to_string()),
    };
    let format = match Path::new(rel).extension().and_then(|e| e.to_str()) {
        Some("json") => Format::Json,
        Some("toml") => Format::Toml,
        _ => return Ok(()),
    };
    let Some(current) = format.schema_version(&existing) else {
        return refuse(format!("its content is not valid {}", format.name()));
    };
    let writing = incoming.and_then(|t| format.schema_version(t)).unwrap_or(1);
    if current > writing {
        return refuse(format!(
            "it was written by a newer clew (schema {current}; this client writes {writing})"
        ));
    }
    Ok(())
}

/// The structured formats [`check_replaceable`] can parse.
#[derive(Clone, Copy)]
enum Format {
    Json,
    Toml,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Json => "JSON",
            Format::Toml => "TOML",
        }
    }

    /// The document's `schema_version` (1 when it has none), or `None` when
    /// the text does not parse at all.
    fn schema_version(self, text: &str) -> Option<u64> {
        match self {
            Format::Json => {
                let value: serde_json::Value = serde_json::from_str(text).ok()?;
                Some(
                    value
                        .get("schema_version")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1),
                )
            }
            Format::Toml => {
                let table: toml::Table = text.parse().ok()?;
                Some(
                    table
                        .get("schema_version")
                        .and_then(|v| v.as_integer())
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(1),
                )
            }
        }
    }
}

/// Spawn the ordered `.clew/` state worker: one task drains the queue and
/// runs each job's (blocking) filesystem work to completion before the next,
/// so state operations apply exactly in request order without ever stalling
/// the request loop.
pub(crate) fn spawn_state_worker(out: UnboundedSender<ServerMessage>) -> UnboundedSender<StateJob> {
    spawn_state_worker_running(out, run_job)
}

/// [`spawn_state_worker`] with the job runner given (a test's may panic).
fn spawn_state_worker_running(
    out: UnboundedSender<ServerMessage>,
    run: fn(StateJob) -> Event,
) -> UnboundedSender<StateJob> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StateJob>();
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let (id, rel) = (job.id, job.rel.clone());
            // Awaited: the next job starts only after this one finished —
            // that ordering is the worker's whole point.
            let event = tokio::task::spawn_blocking(move || run(job)).await;
            // Every job is answered, a panicking one included: the client
            // holds the change as unsaved until it hears back.
            let event = event.unwrap_or_else(|_| {
                failed(format!(
                    "the state operation on .clew/{rel} failed unexpectedly"
                ))
            });
            Server::reply(&out, id, event);
        }
    });
    tx
}

/// One state job, start to finish; the reply it earns.
fn run_job(job: StateJob) -> Event {
    let path = job.root.join(".clew").join(&job.rel);
    let root = job.root.to_string_lossy().into_owned();
    match job.work {
        StateWork::Read => match run_read(&path, &job.rel) {
            Ok(text) => Event::StateContent {
                root,
                rel: job.rel,
                text,
            },
            Err(message) => failed(message),
        },
        // Both write paths answer either way. The client cannot treat a
        // queued frame as a durable write — a dead but undetected transport
        // swallows frames silently — so success has to be as observable as
        // failure.
        StateWork::Write(text) => {
            // Under the store's lock, like every other writer of it: a merge
            // landing between the check and the write would be lost, and a
            // remote edit a crash left pending is settled against the store
            // as it is, before this replaces it — settled after, it would
            // read as not landed, and its replay would apply it twice.
            let _exclusive = match clew_core::statefile::lock(&path) {
                Ok(lock) => lock,
                Err(e) => return failed(format!("lock .clew/{}: {e}", job.rel)),
            };
            // What is on disk may not be replaced (unreadable, unparseable, or
            // from a newer clew): refused, and left as it was.
            if let Err(message) = check_replaceable(&path, &job.rel, text.as_deref()) {
                return refused(message);
            }
            if let Err(e) = clew_core::statefile::settle_pending_edit(&path) {
                return failed(format!("settle .clew/{}: {e}", job.rel));
            }
            let written = match &text {
                Some(text) => clew_core::statefile::write_atomic(&path, text.as_bytes())
                    .map_err(|e| format!("write .clew/{}: {e}", job.rel)),
                None => clew_core::statefile::remove(&path)
                    .map_err(|e| format!("delete .clew/{}: {e}", job.rel)),
            };
            match written {
                Ok(()) => Event::StateWritten { root, rel: job.rel },
                Err(message) => failed(message),
            }
        }
        StateWork::Merge { merge, edit_id } => {
            match merge_or_why(&path, &job.rel, &merge, &edit_id) {
                Ok(text) => Event::StateEdited {
                    root,
                    rel: job.rel,
                    text,
                },
                Err((true, message)) => refused(message),
                Err((false, message)) => failed(message),
            }
        }
    }
}

/// Refuse a state operation whose project is not the one this server holds.
///
/// These writes replace a file wholesale (and delete it when the text is
/// `None`), and the client can only ever have one project open — so a request
/// naming a different root is a save that raced a project switch. Applying it
/// would put one project's bookmarks, trail or tours into another's `.clew/`.
pub(crate) fn wrong_project(root: &Path, want: &str, rel: &str) -> Option<Event> {
    (root.to_string_lossy() != want).then(|| {
        refused(format!(
            "refused: state {rel} is for project {want}, this server has {}",
            root.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        StateJob, StateWork, check_replaceable, run_job, run_merge, run_read,
        spawn_state_worker_running,
    };
    use clew_protocol::{ErrorCode, Event, ServerMessage};
    use std::path::{Path, PathBuf};

    /// The ordered worker answers every job, in request order — one whose
    /// work panics included, with `Failed` under its id — and the jobs after
    /// it still run. The client holds a change as unsaved until it hears
    /// back, so a job that vanished with its panic stranded that change.
    #[tokio::test]
    async fn every_state_job_is_answered_in_order_a_panicking_one_included() {
        fn run(job: StateJob) -> Event {
            if job.rel == "boom.json" {
                panic!("the state job panicked");
            }
            Event::StateWritten {
                root: job.root.to_string_lossy().into_owned(),
                rel: job.rel,
            }
        }
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let jobs = spawn_state_worker_running(out, run);
        for (id, rel) in [(1, "a.json"), (2, "boom.json"), (3, "b.json")] {
            let job = StateJob {
                root: PathBuf::from("/p"),
                rel: rel.into(),
                id,
                work: StateWork::Read,
            };
            assert!(jobs.send(job).is_ok());
        }
        for want in 1..=3 {
            let reply = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("every job is answered")
                .expect("the stream is open");
            let ServerMessage::Reply { id, event } = reply else {
                panic!("expected a reply, got {reply:?}");
            };
            assert_eq!(id, want, "answered in request order");
            match event {
                Event::Error {
                    code: ErrorCode::Failed,
                    message,
                } if id == 2 => {
                    assert!(
                        message.contains(".clew/boom.json failed unexpectedly"),
                        "{message}"
                    )
                }
                Event::StateWritten { .. } if id != 2 => {}
                other => panic!("job {id}: {other:?}"),
            }
        }
    }

    /// A `.clew/` in a fresh directory, removed when dropped.
    struct Project(crate::test_support::Scratch);

    impl Project {
        fn new(tag: &str) -> Project {
            let dir = crate::test_support::Scratch::new(&format!("server-state-{tag}"));
            std::fs::create_dir_all(dir.join(".clew")).unwrap();
            Project(dir)
        }

        fn state(&self, rel: &str) -> PathBuf {
            self.0.join(".clew").join(rel)
        }
    }

    fn bookmark_toggle(rel: &str, line: i64) -> clew_protocol::StateMerge {
        clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "line".into()],
            key: vec![rel.into(), line.into()],
            edit: clew_protocol::StateEdit::Toggle(
                serde_json::json!({"rel": rel, "line": line, "preview": rel}),
            ),
            delete_when_empty: true,
        }
    }

    fn rels_in(path: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(path).unwrap_or_else(|_| "[]".into());
        serde_json::from_str::<Vec<serde_json::Value>>(&text)
            .unwrap()
            .iter()
            .map(|e| e["rel"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Two clients on ONE remote project, each holding the snapshot it loaded
    /// at project open. Both must keep their bookmark: the server is the only
    /// place both writers are visible, so the read-modify-write happens here.
    #[test]
    fn two_divergent_clients_both_keep_their_edit() {
        let project = Project::new("merge");
        let path = project.state("bookmarks.json");
        let start = r#"[{"rel":"a.rs","line":1,"preview":"a.rs"}]"#;
        std::fs::write(&path, start).unwrap();

        // What a whole-snapshot write did: client A adds b.rs, then client B —
        // still holding [a.rs] — adds c.rs and ships its whole list.
        clew_core::statefile::write_atomic(
            &path,
            br#"[{"rel":"a.rs","line":1},{"rel":"c.rs","line":3}]"#,
        )
        .unwrap();
        assert!(
            !rels_in(&path).contains(&"b.rs".to_string()),
            "the wholesale write is what destroyed the other client's bookmark"
        );

        // The same two saves as merges, from the same divergent snapshots.
        std::fs::write(&path, start).unwrap();
        run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "a-1").unwrap();
        run_merge(&path, "bookmarks.json", &bookmark_toggle("c.rs", 3), "b-1").unwrap();
        assert_eq!(rels_in(&path), ["a.rs", "b.rs", "c.rs"]);

        // The reply carries the merged file, so the client stops disagreeing
        // with disk instead of re-sending its own copy.
        let merged = run_merge(&path, "bookmarks.json", &bookmark_toggle("d.rs", 4), "a-2")
            .unwrap()
            .expect("not empty");
        assert_eq!(
            serde_json::from_str::<Vec<serde_json::Value>>(&merged)
                .unwrap()
                .len(),
            4
        );

        // Emptying the store deletes its file, as an empty list does locally.
        for (rel, line) in [("a.rs", 1), ("b.rs", 2), ("c.rs", 3), ("d.rs", 4)] {
            let id = format!("clear-{line}");
            run_merge(&path, "bookmarks.json", &bookmark_toggle(rel, line), &id).unwrap();
        }
        assert!(!path.exists());
    }

    /// fixR1 #13: a client whose transport died before the reply cannot tell
    /// whether its edit landed, so it sends it again — same id — over the next
    /// transport, to what is usually a NEW server process (the old one went
    /// with its SSH session). The ids applied are on disk, so the replay is
    /// answered with the file as it is: the toggle is not undone. A new edit
    /// of the same entry is a new id, and applies.
    #[test]
    fn a_replayed_edit_is_applied_once_whatever_process_sees_it() {
        let project = Project::new("replay");
        let path = project.state("bookmarks.json");
        std::fs::write(&path, r#"[{"rel":"a.rs","line":1,"preview":"a.rs"}]"#).unwrap();

        let first =
            run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "w1-7").unwrap();
        assert_eq!(rels_in(&path), ["a.rs", "b.rs"]);
        // `run_merge` keeps nothing in memory: the replay could come from any
        // server process on this host.
        let replayed =
            run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "w1-7").unwrap();
        assert_eq!(
            rels_in(&path),
            ["a.rs", "b.rs"],
            "the replay undid the toggle"
        );
        assert_eq!(replayed, first, "the replay is answered with the file");
        // Another client's edit in between does not make the replay new.
        run_merge(&path, "bookmarks.json", &bookmark_toggle("c.rs", 3), "w2-1").unwrap();
        run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "w1-7").unwrap();
        assert_eq!(rels_in(&path), ["a.rs", "b.rs", "c.rs"]);
        // The user toggling it again is a new edit.
        run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "w1-8").unwrap();
        assert_eq!(rels_in(&path), ["a.rs", "c.rs"]);
        // The record is clew's own, in the ignored cache — not beside the
        // store, which may be committed.
        assert!(project.state("cache/edits/bookmarks.json").is_file());
        let err = run_merge(
            &path,
            "bookmarks.json",
            &bookmark_toggle("d.rs", 4),
            "no/slash",
        )
        .unwrap_err();
        assert!(err.contains("not an edit id"), "{err}");
    }

    /// A merge the store refuses — content this build cannot understand — is
    /// answered `Refused`, which the client drops; one that failed — here the
    /// edit ledger cannot be written — `Failed`, which it sends again. Both
    /// used to be `Failed`, and the client dropped every one.
    #[test]
    fn a_refused_merge_and_a_failed_one_are_told_apart() {
        use std::os::unix::fs::PermissionsExt;
        let merge = |project: &Project| {
            run_job(StateJob {
                root: project
                    .state("x")
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf(),
                rel: "bookmarks.json".into(),
                id: 1,
                work: StateWork::Merge {
                    merge: bookmark_toggle("b.rs", 2),
                    edit_id: "w7-1".into(),
                },
            })
        };
        let code = |event: Event| match event {
            Event::Error { code, .. } => code,
            other => panic!("expected an error, got {other:?}"),
        };
        let garbled = Project::new("merge-refused");
        std::fs::write(garbled.state("bookmarks.json"), "{ not a store").unwrap();
        assert_eq!(code(merge(&garbled)), ErrorCode::Refused);
        // The failures below are file modes, which root writes and reads past.
        if clew_core::testutil::running_as_root() {
            return;
        }

        let stuck = Project::new("merge-failed");
        let edits = stuck.state("cache/edits");
        std::fs::create_dir_all(&edits).unwrap();
        std::fs::set_permissions(&edits, std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = merge(&stuck);
        std::fs::set_permissions(&edits, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(code(failed), ErrorCode::Failed);

        // A store whose READ failed is not refused by its content either (an
        // unreadable file stands in for EIO, EMFILE, a stale handle): it used
        // to be answered `Refused`, and the edit dropped at its first try.
        let unreadable = Project::new("merge-read-failed");
        let store = unreadable.state("bookmarks.json");
        std::fs::write(&store, "[]").unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o000)).unwrap();
        let failed = merge(&unreadable);
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(code(failed), ErrorCode::Failed);
    }

    /// A wholesale `WriteState` settles a remote edit a crash left pending
    /// before it replaces the store: the edit's replay is then recognised,
    /// and does not toggle the bookmark back off.
    #[test]
    fn a_wholesale_write_settles_a_pending_edit_before_replacing_the_store() {
        let project = Project::new("write-settles");
        let path = project.state("bookmarks.json");
        clew_core::testutil::merge_crashing_after_store_write(
            &path,
            &bookmark_toggle("b.rs", 2),
            "w9-1",
        )
        .unwrap_err();
        let text = r#"[{"rel":"b.rs","line":2,"preview":"b.rs"},{"rel":"c.rs","line":3,"preview":"c.rs"}]"#;
        let written = run_job(StateJob {
            root: path.parent().unwrap().parent().unwrap().to_path_buf(),
            rel: "bookmarks.json".into(),
            id: 1,
            work: StateWork::Write(Some(text.into())),
        });
        assert!(matches!(written, Event::StateWritten { .. }), "{written:?}");
        run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "w9-1").unwrap();
        assert_eq!(
            rels_in(&path),
            ["b.rs", "c.rs"],
            "the replay undid the toggle"
        );
    }

    /// A store this build cannot parse — a typo in a hand edit, a merge
    /// conflict marker — is not "empty": merging into it used to write a
    /// one-entry file over everything it held.
    #[test]
    fn a_merge_into_a_store_it_cannot_parse_leaves_it_alone() {
        let project = Project::new("merge-garbage");
        let path = project.state("bookmarks.json");
        let garbage = "[{\"rel\":\"a.rs\",\"line\":1},\n<<<<<<< HEAD\n";
        std::fs::write(&path, garbage).unwrap();
        let err =
            run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2), "g-1").unwrap_err();
        assert!(err.contains("bookmarks.json"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            garbage,
            "untouched"
        );
    }

    /// Refused is not missing: a read the statefile rules refuse is an error,
    /// not an empty store the client may then save over.
    #[cfg(unix)]
    #[test]
    fn a_refused_read_is_an_error_and_a_missing_one_is_not() {
        let project = Project::new("read");
        assert_eq!(
            run_read(&project.state("history.json"), "history.json"),
            Ok(None)
        );

        let outside = project.0.join("outside.json");
        std::fs::write(&outside, "[]").unwrap();
        std::os::unix::fs::symlink(&outside, project.state("bookmarks.json")).unwrap();
        let err = run_read(&project.state("bookmarks.json"), "bookmarks.json").unwrap_err();
        assert!(err.contains("cannot read .clew/bookmarks.json"), "{err}");

        std::fs::write(project.state("notes.json"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(
            run_read(&project.state("notes.json"), "notes.json").is_err(),
            "not UTF-8"
        );
    }

    /// The wholesale write is last-writer-wins only among files this build
    /// understands; anything else is left for the user to repair.
    #[test]
    fn a_wholesale_write_never_replaces_what_it_cannot_parse() {
        let project = Project::new("replace");
        let history = project.state("history.json");
        let new = Some(r#"{"schema_version":1,"nodes":[],"current":null}"#);

        // No file yet, or a file this build wrote: replaceable.
        assert!(check_replaceable(&history, "history.json", new).is_ok());
        std::fs::write(&history, r#"{"schema_version":1,"nodes":[]}"#).unwrap();
        assert!(check_replaceable(&history, "history.json", new).is_ok());
        // A legacy file without the field counts as schema 1.
        std::fs::write(&history, r#"{"nodes":[]}"#).unwrap();
        assert!(check_replaceable(&history, "history.json", new).is_ok());

        // Not JSON at all.
        std::fs::write(&history, r#"{"nodes": [ <<< broken"#).unwrap();
        let err = check_replaceable(&history, "history.json", new).unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        // A newer clew's layout — for a write and for a delete alike.
        std::fs::write(&history, r#"{"schema_version":2,"trail":[]}"#).unwrap();
        let err = check_replaceable(&history, "history.json", new).unwrap_err();
        assert!(err.contains("newer clew"), "{err}");
        assert!(check_replaceable(&history, "history.json", None).is_err());
        // ...unless the writer is at least as new.
        assert!(
            check_replaceable(&history, "history.json", Some(r#"{"schema_version":2}"#)).is_ok()
        );

        // TOML is parsed too.
        let reading = project.state("reading.toml");
        std::fs::write(&reading, "rel = \"src/lib.rs\"\n").unwrap();
        assert!(check_replaceable(&reading, "reading.toml", Some("rel = \"a\"\n")).is_ok());
        std::fs::write(&reading, "rel = \"unterminated\n").unwrap();
        assert!(check_replaceable(&reading, "reading.toml", Some("rel = \"a\"\n")).is_err());
    }
}
