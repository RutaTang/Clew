//! Headless-App regression tests: drive Messages through update() and assert state.

use crate::app::prelude::*;
use crate::finder::FinderMode;
use crate::*;
use iced::keyboard;

#[test]
fn dart_fn_detail_extracts_full_body_not_duplicated_header() {
    // A doc-commented Dart block function: Dart tags only the signature line,
    // so without the brace-extension the "body" would be the header twice.
    let dir = std::env::temp_dir().join("clew-dart-detail-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("calc.dart");
    std::fs::write(
            &file,
            "/// Parse everything.\ndouble parseAll(int x) {\n  var e = x + 1;\n  return e.toDouble();\n}\n",
        )
        .unwrap();
    let (sig, body, _) =
        gather_fn_detail_input(&dir, file, "parseAll", 0, &HashMap::new()).expect("detail");
    assert!(sig.contains("parseAll"));
    assert!(
        body.contains("var e = x + 1"),
        "body missing statements: {body:?}"
    );
    assert!(
        body.contains("return e.toDouble()"),
        "body missing return: {body:?}"
    );
    // The header must appear once in the body, not duplicated.
    assert_eq!(
        body.matches("parseAll").count(),
        1,
        "duplicated header: {body:?}"
    );
}

#[test]
fn fn_body_end_matches_and_handles_nested_and_bodyless() {
    // Signature line + block body → reach the closing brace on line 4.
    let lines = [
        "Expr parseAll() {",
        "  var e = expr();",
        "  return e;",
        "}",
        "otherFn()",
    ];
    assert_eq!(fn_body_end(&lines, 0), Some(4)); // lines[0..4] = the function
    // Nested braces are balanced correctly.
    let nested = ["fn f() {", "  if x { g(); }", "}"];
    assert_eq!(fn_body_end(&nested, 0), Some(3));
    // No brace (expression-bodied) → None (caller keeps the single line).
    assert_eq!(fn_body_end(&["double get m => x;"], 0), None);
}

#[test]
fn fn_body_end_skips_dart_named_parameter_braces() {
    // A Dart multi-line signature whose named parameters use `{ }` *inside*
    // the parens. Naive brace matching stops at the named-parameter `}` on
    // line 4 and returns just the signature; fn_body_end must skip those and
    // reach the real body's closing brace on line 7.
    let lines = [
        "Future<void> initializeRust(",               // 0
        "  AssignRustSignal<String, dynamic> sig, {", // 1  (named-param '{')
        "  String? compiledLibPath,",                 // 2
        "}) async {",                                 // 3  ('}' closes params, '{' opens body)
        "  if (compiledLibPath != null) {",           // 4
        "    setPath(compiledLibPath);",              // 5
        "  }",                                        // 6
        "}",                                          // 7  body close
        "void next() {}",                             // 8
    ];
    assert_eq!(fn_body_end(&lines, 0), Some(8)); // lines[0..8] = the whole function
    // A single-line signature + body still works.
    assert_eq!(fn_body_end(&["fn f() {", "  g();", "}"], 0), Some(3));
    // A bodyless declaration (abstract / trait signature) → None.
    assert_eq!(fn_body_end(&["void doThing(int a);"], 0), None);
}

/// The throwaway data directory the tests in this file run against.
///
/// `clew_core::lsp::store::data_root()` falls back to the developer's real
/// `~/Library/Application Support/clew` whenever `CLEW_DATA_DIR` is unset, and
/// these tests reach it constantly: `App::blank()` loads `trust.toml` and
/// `connections.toml` out of it, and every `ScanDone` `create_dir_all`s
/// `<data_root>/cache/<project key>` and warm-loads the explain, overview,
/// stats and embedding artifacts from there. Only eight tests here set the
/// variable, so the ~90 that do not used to read and write the user's live
/// data: a single run left 91 directories under the real cache, one of them
/// holding an `explain.json` a LATER run warm-loaded, which makes an
/// assertion's outcome a function of the developer's home directory rather
/// than of the code under test.
///
/// Isolation is therefore the default and not an opt-in: `blank_app()` and
/// `fixture_project()` are the two doors every test in this file goes through
/// and both prime this, so a test that never mentions `CLEW_DATA_DIR` still
/// lands in temp. Any inherited value is overridden unconditionally, so a
/// `CLEW_DATA_DIR` exported in the developer's shell cannot aim the suite back
/// at data that matters.
///
/// Priming takes the env lock, which is not reentrant: reaching this while
/// already holding that lock hangs the run. Nothing in this file locks by
/// hand for that reason — `env_lock()` below primes first, and every other
/// caller goes through it.
///
/// Two gaps this does NOT close, both outside this file and neither of them a
/// reason to leave the ~90 tests where they were. `src/shell.rs`'s tests build
/// their `App`s directly, so they still read the real `trust.toml` and
/// `connections.toml` (they only read). And `updates_stage_under_the_data_root_…`
/// in `src/macos/install.rs` deliberately unsets the variable for its body, to
/// assert about the shipped default: it holds the env lock while it does, but
/// the tests here read `data_root()` without that lock, so under a parallel
/// `cargo test` one of them can still land in that window and create a cache
/// directory under the real root. `--test-threads=1` has no such window.
fn isolated_data_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("clew-app-test-data-{}", std::process::id()));
        // Start empty: a previous run that happened to reuse this pid would
        // otherwise have its leftovers warm-loaded as this run's own work.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _env = clew_core::env_lock();
        // SAFETY: env mutation serialized by env_lock.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        dir
    })
    .as_path()
}

/// `clew_core::env_lock()` as the tests in this file must take it: the
/// isolated data directory is primed BEFORE the lock, because priming takes
/// that same non-reentrant lock. A test that called `clew_core::env_lock()`
/// directly and only then reached `fixture_project()`, `blank_app()` or
/// `isolated_data_dir()` would deadlock the run if it happened to be the first
/// to prime, so this shadow is the only door.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    isolated_data_dir();
    clew_core::env_lock()
}

/// Build the headless App the tests drive, with the isolated data directory
/// already in place. Tests use this and never `App::blank()` directly:
/// `blank()` loads `trust.toml` and `connections.toml` through `data_root()`
/// while it runs, so a bare call reads the developer's real clew data.
fn blank_app() -> App {
    isolated_data_dir();
    App::blank()
}

/// Points `CLEW_DATA_DIR` at `dir` for the rest of the enclosing scope, holding
/// the process-global env lock for that whole time so no other env-touching
/// test observes the change.
///
/// On drop it restores the suite's isolated default instead of unsetting the
/// variable: `remove_var` would send every OTHER test in this file — the ~90
/// that never mention `CLEW_DATA_DIR` — back to the developer's real data
/// directory for the rest of the run. Restoring on drop also covers the case
/// the trailing `remove_var` these tests used to end with did not: a failing
/// assertion unwinds past it and leaves the override in place.
#[must_use = "the override lasts only as long as the returned guard"]
fn data_dir_override(dir: &Path) -> DataDirOverride {
    let env = env_lock();
    // SAFETY: env mutation serialized by env_lock, which the guard keeps held.
    unsafe { std::env::set_var("CLEW_DATA_DIR", dir) };
    DataDirOverride { _env: env }
}

struct DataDirOverride {
    _env: std::sync::MutexGuard<'static, ()>,
}

impl Drop for DataDirOverride {
    fn drop(&mut self) {
        // SAFETY: the field drops after this body, so env_lock is still held
        // and no other env-touching test can be running.
        unsafe { std::env::set_var("CLEW_DATA_DIR", isolated_data_dir()) };
    }
}

/// The suite must never run against the developer's real clew data directory.
/// It used to: `data_root()` falls back to `$HOME`, and the ~90 tests here that
/// never mention `CLEW_DATA_DIR` took that fallback, creating a cache directory
/// per fixture under the user's live data and warm-loading whatever an earlier
/// run had left there.
#[test]
fn the_app_tests_run_against_an_isolated_data_directory() {
    // Hold the env lock while sampling: a `data_dir_override` test running in
    // parallel legitimately has the variable pointed at its own directory, and
    // reading it mid-override would see that instead of the suite default.
    let _env = env_lock();
    let root = clew_core::lsp::store::data_root().expect("a data root");
    assert_eq!(
        root,
        isolated_data_dir(),
        "the tests are not pointed at the isolated data directory"
    );
    // `data_root()`'s fallback is always somewhere under `$HOME` (on macOS
    // `Library/Application Support/clew`), and the isolated directory lives in
    // the temp dir, so this catches the fallback being taken at all.
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    assert!(
        !root.starts_with(&home),
        "the tests write inside the developer's home: {}",
        root.display()
    );
}

/// A test that points `CLEW_DATA_DIR` somewhere of its own must hand the
/// variable back to the suite's isolated default, not unset it: unsetting it
/// returns every LATER test in this file to the `$HOME` fallback, which is the
/// exact leak the isolation exists to prevent.
#[test]
fn a_data_dir_override_hands_back_the_isolated_default() {
    let dir = std::env::temp_dir().join("clew-app-test-override-restore");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    {
        let _env = data_dir_override(&dir);
        assert_eq!(
            clew_core::lsp::store::data_root().expect("a data root"),
            dir,
            "the override did not take effect"
        );
    }
    let _env = env_lock();
    assert_eq!(
        clew_core::lsp::store::data_root().expect("a data root"),
        isolated_data_dir(),
        "dropping the override left the suite pointed somewhere else"
    );
}

/// Each test gets its own directory: tests run in parallel and would
/// otherwise race on remove_dir_all/create of a shared fixture.
fn fixture_project(tag: &str) -> PathBuf {
    isolated_data_dir();
    let dir = std::env::temp_dir().join(format!("clew-app-test-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n",
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), "needle in notes\n").unwrap();
    dir.canonicalize().unwrap()
}

/// Drive the update loop the way the runtime would, executing the
/// blocking parts inline instead of through iced Tasks.
fn open_synchronously(app: &mut App, rel: &str, line: Option<usize>) {
    let abs = app.project.as_ref().unwrap().root.join(rel);
    let pane = app.active;
    let _ = app.update(Message::OpenRel {
        rel: rel.to_string(),
        line,
    });
    let content = read_text_file(&abs).unwrap();
    let req = app.pane_pending[pane].expect("open_file minted a load token");
    let _ = app.update(Message::FileLoaded {
        req,
        pane,
        abs: abs.clone(),
        target: line,
        result: Ok(content.clone()),
    });
    let lang = highlight::detect(&abs);
    let lines = highlight::highlight_lines(&content, lang);
    let symbols = lang
        .map(|k| outline::extract(&content, k))
        .unwrap_or_default();
    let docs = lang
        .map(|k| docs::extract(&content, k, &symbols))
        .unwrap_or_default();
    let inactive = lang
        .map(|k| inactive::inactive_lines(&content, k, &inactive::Target::host()))
        .unwrap_or_default();
    let _ = app.update(Message::Highlighted {
        abs,
        src_hash: incremental::content_hash(content.as_bytes()),
        lines,
        symbols,
        docs,
        inactive,
        target: inactive::Target::host(),
    });
}

/// Feed a scan result the way the runtime would: `ScanDone` is accepted only
/// for a scan the app is actually waiting on (see the `ScanDone` arm).
fn scan_synchronously(app: &mut App, root: PathBuf) {
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
}

fn scanned_app(tag: &str) -> App {
    let root = fixture_project(tag);
    let mut app = blank_app();
    scan_synchronously(&mut app, root);
    app
}

/// Build the message the off-thread rehash sends back for `events`: stamped
/// with the project it was computed for and with the registry versions each
/// path was hashed against, exactly as `on_files_changed` collects them (an
/// untracked path baselines at `0`).
fn rehashed(app: &App, events: Vec<watch::FileEvent>) -> Message {
    let baselines = events
        .iter()
        .map(|e| {
            let path = match e {
                watch::FileEvent::Modified(c) => c.path.clone(),
                watch::FileEvent::Deleted(p) => p.clone(),
            };
            let version = app.registry.version(&path).unwrap_or(0);
            (path, version)
        })
        .collect();
    Message::FilesRehashed {
        root: app.project.as_ref().unwrap().root.clone(),
        epoch: app.project_epoch,
        events,
        baselines,
        fs_structural: false,
    }
}

/// A modification event for `path` carrying `content` under `hash`.
fn modified(path: &Path, hash: incremental::Version, content: &str) -> watch::FileEvent {
    watch::FileEvent::Modified(watch::Changed {
        path: path.to_path_buf(),
        hash,
        content: std::sync::Arc::new(content.to_string()),
    })
}

#[test]
fn full_reading_flow() {
    let mut app = scanned_app("reading");
    assert!(app.project.is_some());
    assert_eq!(app.project.as_ref().unwrap().files.len(), 2);

    // Open a file at a line.
    open_synchronously(&mut app, "src/lib.rs", Some(3));
    let v = app.active_viewer().unwrap();
    assert_eq!(v.rel, "src/lib.rs");
    assert!(v.highlighted);
    assert_eq!(v.target_line, Some(3));
    assert_eq!(v.lines.len(), 5);

    // Outline extracted for the current file.
    let names: Vec<&str> = v.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"origin"), "outline: {names:?}");

    // Open a second file, then navigate back and forward.
    open_synchronously(&mut app, "notes.txt", None);
    assert_eq!(app.active_viewer().unwrap().rel, "notes.txt");
    assert!(app.history.can_back());

    let back = app.history.back().unwrap();
    assert!(back.path.ends_with("src/lib.rs"));
    assert_eq!(back.line, Some(3));
    let fwd = app.history.forward().unwrap();
    assert!(fwd.path.ends_with("notes.txt"));
}

// ---- Explain-domain handler regressions (guard the eval-campaign fixes) ---

#[test]
fn reexplain_on_unexplained_node_does_not_start_a_project_pass() {
    // Fix: a single "Re-explain" click on a never-explained node must NOT
    // kick off the whole-project pass (thousands of LLM calls) — it should
    // point the user at the explicit Explain-All instead.
    let mut app = scanned_app("reexplain-guard");
    app.llm_available = true; // else it returns early on a missing key
    app.explain.view = Some(explain::Node::Function {
        file: app.project.as_ref().unwrap().root.join("src/lib.rs"),
        name: "origin".into(),
        ordinal: 0,
    });
    assert!(app.explain.cache.is_empty());
    let _ = app.update(Message::ReexplainNode);
    assert!(
        !app.explain.running,
        "must not start a project pass on an unexplained node"
    );
    assert!(
        app.status.contains("Nothing to re-explain"),
        "status: {}",
        app.status
    );
}

#[test]
fn cancel_explain_stops_and_clears_progress() {
    // Fix: a running Explain pass must be cancellable.
    let mut app = scanned_app("cancel-explain");
    app.explain.running = true;
    app.explain.progress = Some((3, 10));
    let _ = app.update(Message::CancelExplain);
    assert!(!app.explain.running);
    assert_eq!(app.explain.progress, None);
    assert!(app.status.contains("cancelled"), "status: {}", app.status);
}

#[test]
fn explain_done_from_a_stale_generation_is_ignored() {
    // A result from a superseded pass (older generation) must be dropped, so
    // a cancelled/restarted pass can't be clobbered by a late arrival.
    let mut app = scanned_app("explain-done-stale");
    let root = app.project.as_ref().unwrap().root.clone();
    app.explain.running = true;
    app.explain.generation = 5;
    let _ = app.update(Message::ExplainDone {
        root,
        generation: 4, // stale
        cache: explain::Cache::new(),
        failed: 0,
        auth_error: None,
    });
    assert!(
        app.explain.running,
        "a stale ExplainDone must not clear the running flag"
    );
}

#[test]
fn finder_flow() {
    let mut app = scanned_app("finder");

    let _ = app.update(Message::FinderOpened(FinderMode::Files));
    assert!(app.finder.open);
    assert!(!app.finder.results.is_empty());

    let _ = app.update(Message::FinderQueryChanged("librs".to_string()));
    let files = app.project.as_ref().unwrap().files.clone();
    let top = files[app.finder.results[0]].rel.clone();
    assert_eq!(top, "src/lib.rs");

    // Confirm closes the finder.
    let _ = app.update(Message::FinderConfirm);
    assert!(!app.finder.open);
}

#[test]
fn incremental_reindex_on_change_and_delete() {
    let mut app = scanned_app("reindex");
    let files = app.project.as_ref().unwrap().files.clone();
    let root = app.project.as_ref().unwrap().root.clone();
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files),
    });
    let abs = app.project.as_ref().unwrap().root.join("src/lib.rs");
    assert!(app.symbol_index.iter().any(|e| e.name == "origin"));
    assert!(app.registry.version(&abs).is_some());

    // An external edit that renames the function re-indexes just that file.
    let ev = modified(&abs, 424242, "pub fn renamed() -> u8 {\n    1\n}\n");
    let msg = rehashed(&app, vec![ev]);
    let _ = app.update(msg);
    assert!(app.symbol_index.iter().any(|e| e.name == "renamed"));
    assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
    assert_eq!(app.registry.version(&abs), Some(424242));

    // Deleting the file drops its symbols and forgets its version.
    let msg = rehashed(&app, vec![watch::FileEvent::Deleted(abs.clone())]);
    let _ = app.update(msg);
    assert!(!app.symbol_index.iter().any(|e| e.name == "renamed"));
    assert_eq!(app.registry.version(&abs), None);
    assert!(!app.symbol_index_by_file.contains_key(&abs));
}

#[test]
fn structure_index_follows_edits_and_ignores_a_build_that_read_older_bytes() {
    // The hover peek's "impl …" line comes from a project-wide index built
    // off-thread. Built only once, after the initial symbol index, it kept
    // answering with the relations the project had at open for the rest of the
    // session: a watcher batch that touches Rust source must queue a rebuild.
    // And once rebuilds exist, two of them can be in flight, so the result has
    // to name the source revision it read — arriving last is not evidence of
    // being the newest.
    let mut app = scanned_app("structure-rebuild");
    let root = app.project.as_ref().unwrap().root.clone();
    let files = app.project.as_ref().unwrap().files.clone();
    let abs = root.join("src/lib.rs");
    std::fs::write(&abs, "pub struct Point;\nimpl Alpha for Point {}\n").unwrap();
    // Indexing seeds the registry (so the edit below reads as a content change,
    // not a creation) and spawns the first structure build.
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files.clone()),
    });
    assert!(app.structure_building, "no build spawned after indexing");
    let _ = app.update(Message::StructureBuilt {
        root: root.clone(),
        epoch: app.project_epoch,
        rev: app.registry.revision(),
        index: structure::build(&root, &files),
    });
    assert_eq!(
        app.structure.summary_line("Point").as_deref(),
        Some("impl Alpha")
    );

    // An external edit swaps the trait: the batch queues a rebuild…
    let src = "pub struct Point;\nimpl Beta for Point {}\n";
    std::fs::write(&abs, src).unwrap();
    let msg = rehashed(&app, vec![modified(&abs, 424242, src)]);
    let _ = app.update(msg);
    assert!(
        app.structure_building,
        "a changed Rust file must queue a structure rebuild"
    );

    // …and its result replaces the pre-edit relations.
    let rev = app.registry.revision();
    let _ = app.update(Message::StructureBuilt {
        root: root.clone(),
        epoch: app.project_epoch,
        rev,
        index: structure::build(&root, &files),
    });
    assert_eq!(
        app.structure.summary_line("Point").as_deref(),
        Some("impl Beta")
    );

    // A build that read the file BEFORE that edit, landing after it, must not
    // put the old relations back for the rest of the session.
    let _ = app.update(Message::StructureBuilt {
        root,
        epoch: app.project_epoch,
        rev: rev - 1,
        index: structure::StructureIndex::default(),
    });
    assert_eq!(
        app.structure.summary_line("Point").as_deref(),
        Some("impl Beta")
    );
}

#[test]
fn opening_another_project_drops_the_previous_structure_index() {
    // Every other derived artifact is reset at project open; the structure
    // index was not, so until the new project's first build landed the hover
    // peek answered with the OLD project's impls for any name they share.
    let mut app = scanned_app("structure-switch");
    let root = app.project.as_ref().unwrap().root.clone();
    let files = app.project.as_ref().unwrap().files.clone();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct Point;\nimpl Alpha for Point {}\n",
    )
    .unwrap();
    let _ = app.update(Message::StructureBuilt {
        root: root.clone(),
        epoch: app.project_epoch,
        rev: app.registry.revision(),
        index: structure::build(&root, &files),
    });
    assert!(app.structure.summary_line("Point").is_some());

    scan_synchronously(&mut app, fixture_project("structure-switch-other"));
    assert!(
        app.structure.is_empty(),
        "the previous project's relations survived the switch"
    );
}

#[test]
fn rehash_from_a_previous_project_is_ignored() {
    // A rehash is computed off-thread, so it can land after the user opened
    // another project. It writes straight into the registry, symbol index and
    // import graph, so applying it here would file the OLD project's paths
    // under the new one (and buy a tree rescan plus an LLM auto-refresh pass
    // with them). Only the (root, epoch) stamp can stop it: the baseline
    // compare-and-swap cannot, because the new project does not track this
    // path either, so both baselines read as "untracked".
    let mut app = scanned_app("rehash-stale-epoch");
    let root = app.project.as_ref().unwrap().root.clone();
    let epoch = app.project_epoch;
    let old_abs = root.join("src/lib.rs");
    let events = vec![modified(&old_abs, 424242, "pub fn leaked() {}\n")];

    // The user switches projects while that batch is still running.
    scan_synchronously(&mut app, fixture_project("rehash-stale-epoch-other"));
    assert!(app.registry.version(&old_abs).is_none());
    let _ = app.update(Message::FilesRehashed {
        root,
        epoch,
        events,
        baselines: [(old_abs.clone(), 0)].into_iter().collect(),
        fs_structural: false,
    });

    assert_eq!(
        app.registry.version(&old_abs),
        None,
        "the previous project's file must not enter this project's registry"
    );
    assert!(
        !app.symbol_index.iter().any(|e| e.name == "leaked"),
        "the previous project's symbols must not enter this project's index"
    );
    assert!(!app.symbol_index_by_file.contains_key(&old_abs));
}

#[test]
fn an_out_of_order_rehash_does_not_roll_a_file_back() {
    // Two rehash batches can be in flight over the same file, and they do not
    // complete in read order — a large batch reads a file early and lands
    // late. The batch that arrives second here read the OLDER bytes, so
    // applying it would roll the registry and the symbol index back to them.
    let mut app = scanned_app("rehash-out-of-order");
    let root = app.project.as_ref().unwrap().root.clone();
    let files = app.project.as_ref().unwrap().files.clone();
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files),
    });
    let abs = root.join("src/lib.rs");

    // Both batches were dispatched against the same recorded version.
    let older = rehashed(&app, vec![modified(&abs, 111, "pub fn older() {}\n")]);
    let newer = rehashed(&app, vec![modified(&abs, 222, "pub fn newer() {}\n")]);

    let _ = app.update(newer);
    assert_eq!(app.registry.version(&abs), Some(222));
    assert!(app.symbol_index.iter().any(|e| e.name == "newer"));

    // The older batch lands afterwards: its baseline is no longer what the
    // registry holds, so nothing of it applies.
    let _ = app.update(older);
    assert_eq!(
        app.registry.version(&abs),
        Some(222),
        "an older read must not roll the recorded version back"
    );
    assert!(
        app.symbol_index.iter().any(|e| e.name == "newer"),
        "the newer symbols must survive the late batch"
    );
    assert!(!app.symbol_index.iter().any(|e| e.name == "older"));
}

#[test]
fn an_edit_during_the_initial_index_survives_the_index_result() {
    // The initial index is built off-thread from the tree as it stood when the
    // project opened, and the watcher keeps applying edits for as long as that
    // build runs. A result landing after one of those edits carries the
    // pre-edit bytes for that file: applied wholesale it reverts the file, and
    // the revert then STICKS, because restoring the file to exactly those
    // bytes is what the watcher's `hash == old` filter drops as a no-op.
    let root = fixture_project("index-window");
    std::fs::write(root.join("src/util.rs"), "pub fn untouched_helper() {}\n").unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());

    // The background build reads the tree as it is right now; its result is
    // held back until after the edit below, as the runtime's would be.
    let files = app.project.as_ref().unwrap().files.clone();
    let indexed = index::build_indexed(&root, files);

    // Meanwhile the watcher picks up an edit to one file and applies it, with
    // that file open in a pane.
    let abs = root.join("src/lib.rs");
    open_synchronously(&mut app, "src/lib.rs", None);
    let edited = "pub fn edited_while_indexing() {}\n";
    let hash = incremental::content_hash(edited.as_bytes());
    let msg = rehashed(&app, vec![modified(&abs, hash, edited)]);
    let _ = app.update(msg);
    assert_eq!(app.registry.version(&abs), Some(hash));

    // Only now does the index result arrive.
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed,
    });

    let shown = app.active_viewer().unwrap().source.clone();
    assert_eq!(
        app.registry.version(&abs),
        Some(incremental::content_hash(shown.as_bytes())),
        "the registry must still describe the bytes the reader is looking at, \
         or the watcher can no longer tell a revert from a no-op"
    );
    assert!(
        app.symbol_index
            .iter()
            .any(|e| e.name == "edited_while_indexing"),
        "the edited file keeps the symbols extracted from the newer bytes"
    );
    assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
    // The result is merged per file, not dropped whole: everything the window
    // did not touch still gets its index.
    assert!(
        app.symbol_index
            .iter()
            .any(|e| e.name == "untouched_helper"),
        "files untouched during the indexing window must still be indexed"
    );
    assert!(app.registry.version(&root.join("src/util.rs")).is_some());
}

#[test]
fn a_file_deleted_during_the_initial_index_is_not_resurrected_by_the_result() {
    // The other half of the same window: the build read the file before it was
    // deleted, so its symbols and imports are in the result. The registry
    // cannot object — it keeps no tombstone, so the deleted path looks exactly
    // like one never seen and gets seeded — and nothing prunes the index
    // afterwards, so the dead file kept its symbols in the finder (opening one
    // then fails on a path that is gone) and its node in the import graph for
    // the rest of the session. The live file set is the only evidence of the
    // deletion, so the result is filtered against it too.
    let root = fixture_project("index-window-delete");
    std::fs::write(root.join("src/util.rs"), "pub fn untouched_helper() {}\n").unwrap();
    let doomed = root.join("src/gone.rs");
    std::fs::write(&doomed, "use crate::util;\n\npub fn doomed_helper() {}\n").unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());

    // The background build reads the tree as it stands right now — gone.rs
    // included — and its result is held back the way the runtime's would be.
    let files = app.project.as_ref().unwrap().files.clone();
    let indexed = index::build_indexed(&root, files);
    assert!(indexed.by_file.contains_key(&doomed));
    assert!(indexed.imports_by_file.contains_key(&doomed));

    // Meanwhile the file is deleted and the watcher's rescan splices in a tree
    // that no longer lists it.
    std::fs::remove_file(&doomed).unwrap();
    let _ = app.update(Message::TreeUpdated {
        epoch: app.project_epoch,
        result: fs_scan::scan(root.clone()),
    });
    assert!(
        !app.project
            .as_ref()
            .unwrap()
            .files
            .iter()
            .any(|f| f.abs == doomed),
        "the rescan must have dropped the deleted file"
    );

    // Only now does the index result arrive.
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed,
    });

    assert!(
        !app.symbol_index.iter().any(|e| e.name == "doomed_helper"),
        "a file deleted during the indexing window must not come back in the symbol index"
    );
    assert!(!app.symbol_index_by_file.contains_key(&doomed));
    assert_eq!(
        app.registry.version(&doomed),
        None,
        "seeding a version for a deleted path also makes a later re-creation \
         read as a plain edit instead of a new source file"
    );
    assert!(
        !app.import_graph.files().contains(&doomed),
        "the deleted file must not be a node in the import graph"
    );
    assert!(
        !app.import_graph
            .importers(&root.join("src/util.rs"))
            .contains(&doomed),
        "the deleted file's out-edges must not come back either"
    );
    // Still merged per file: everything that survived the window is indexed.
    assert!(
        app.symbol_index
            .iter()
            .any(|e| e.name == "untouched_helper"),
        "filtering the deleted file must not drop the files that still exist"
    );
    assert!(app.symbol_index.iter().any(|e| e.name == "origin"));
    assert!(app.registry.version(&root.join("src/util.rs")).is_some());
}

#[test]
fn opening_a_file_while_it_is_being_rehashed_still_refreshes_its_index() {
    // Opening a file writes the registry too: the load records the bytes it
    // just read and derives nothing else from them. So the reader clicking the
    // file a batch is already reading moves the registry to exactly the bytes
    // that batch carries — refusing the batch there protects nothing from a
    // rollback, it throws away the only re-index, import refresh and trail
    // re-anchor those bytes will ever get, since the watcher drops the next
    // read of a file it believes is already recorded.
    let mut app = scanned_app("rehash-open-race");
    let root = app.project.as_ref().unwrap().root.clone();
    let files = app.project.as_ref().unwrap().files.clone();
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files),
    });
    let abs = root.join("src/lib.rs");
    assert!(app.symbol_index.iter().any(|e| e.name == "origin"));

    // The batch goes out against the indexed version…
    let edited = "pub fn edited_on_disk() -> u8 {\n    1\n}\n";
    let hash = incremental::content_hash(edited.as_bytes());
    let msg = rehashed(&app, vec![modified(&abs, hash, edited)]);

    // …and before it lands the reader opens that file, whose load records the
    // very same bytes.
    std::fs::write(&abs, edited).unwrap();
    open_synchronously(&mut app, "src/lib.rs", None);
    assert_eq!(app.registry.version(&abs), Some(hash));

    let _ = app.update(msg);
    assert!(
        app.symbol_index.iter().any(|e| e.name == "edited_on_disk"),
        "a read the registry already agrees with must still refresh the index"
    );
    assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
}

#[test]
fn a_superseded_rehash_is_re_read_instead_of_dropped() {
    // Two batches over the same file landing in dispatch order: the second one
    // read the file later, so it carries the NEWER bytes, but its baseline is
    // the version the first has just replaced. It cannot be applied on trust
    // (an older read landing late is indistinguishable from here), and
    // dropping it loses the edit for the session — the watcher's `hash == old`
    // filter means a file that has settled produces no further event. The
    // refusal has to schedule a fresh read of what is actually on disk.
    let mut app = scanned_app("rehash-superseded");
    let root = app.project.as_ref().unwrap().root.clone();
    let files = app.project.as_ref().unwrap().files.clone();
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files),
    });
    let abs = root.join("src/lib.rs");

    // Both batches were dispatched against the indexed version.
    let first = rehashed(&app, vec![modified(&abs, 111, "pub fn first() {}\n")]);
    let second = rehashed(&app, vec![modified(&abs, 222, "pub fn second() {}\n")]);

    let _ = app.update(first);
    assert_eq!(app.registry.version(&abs), Some(111));
    assert!(!app.registry.is_reading(&abs));

    let _ = app.update(second);
    assert_eq!(
        app.registry.version(&abs),
        Some(111),
        "an unproven read must not be applied on trust either"
    );
    assert!(
        app.registry.is_reading(&abs),
        "the refused path must be read again, or its newer bytes are lost for the session"
    );
}

#[test]
fn a_refused_deletion_is_re_read_instead_of_dropped() {
    // The same overlap ending in a deletion. A stale delete must not erase a
    // file a later batch re-created, so a moved registry still refuses it —
    // but a deleted path produces no further watcher event, so unless the
    // refusal schedules a re-read the file keeps its symbols, its graph node
    // and its registry entry for the whole session while the tree drops it.
    let mut app = scanned_app("rehash-refused-delete");
    let root = app.project.as_ref().unwrap().root.clone();
    let files = app.project.as_ref().unwrap().files.clone();
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files),
    });
    let abs = root.join("src/lib.rs");

    // Both dispatched against the indexed version; the modification lands
    // first, so the deletion's baseline is stale by the time it arrives.
    let edit = rehashed(&app, vec![modified(&abs, 111, "pub fn first() {}\n")]);
    let delete = rehashed(&app, vec![watch::FileEvent::Deleted(abs.clone())]);
    let _ = app.update(edit);
    let _ = app.update(delete);

    assert_eq!(
        app.registry.version(&abs),
        Some(111),
        "a delete whose baseline moved must not erase what the newer read installed"
    );
    assert!(app.symbol_index_by_file.contains_key(&abs));
    assert!(
        app.registry.is_reading(&abs),
        "a refused delete gets no second watcher event, so it must be re-read here"
    );
}

/// An open notebook that changes on disk must be re-read and re-parsed, never
/// reloaded from the raw `.ipynb` bytes. A notebook pane's text is the script
/// projection and its view paints the parsed cells, so a raw reload leaves the
/// pre-edit cells on screen over a JSON line space: the outline empties and
/// every goto or search hit into that pane lands on an arbitrary cell.
#[test]
fn a_changed_notebook_is_re_parsed_instead_of_reloaded_as_raw_json() {
    let mut app = scanned_app("nb-on-disk-change");
    let root = app.project.as_ref().unwrap().root.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);

    let abs = root.join("analysis.ipynb");
    let raw = |call: &str| format!("{{\"cells\":[{{\"source\":\"{call}\"}}]}}\n");
    std::fs::write(&abs, raw("first()")).unwrap();
    let code_cell = |call: &str| clew_protocol::NotebookCell {
        kind: "code".into(),
        source: call.to_string(),
        lines: Vec::new(),
        proj_line: 2,
        outputs: Vec::new(),
        execution_count: None,
    };
    // The server parsed it when the pane opened: cells plus the `# %%`
    // projection the pane's line space actually is.
    let _ = app.apply_notebook_content(
        &[0],
        None,
        "analysis.ipynb".into(),
        "python".into(),
        vec![code_cell("first()")],
        Vec::new(),
        "# %%\nfirst()\n".into(),
        false,
    );
    while rx.try_recv().is_ok() {} // whatever opening sent is not what we assert on

    // Jupyter saves the notebook; the watcher hands the client the RAW bytes.
    let edited = raw("second()");
    std::fs::write(&abs, &edited).unwrap();
    let _ = app.update(rehashed(&app, vec![modified(&abs, 4242, &edited)]));

    let v = app.panes[0].as_ref().expect("the notebook pane");
    assert!(
        !v.source.contains("\"cells\""),
        "the pane's line space must stay the projection, not the file's JSON: {}",
        v.source
    );
    assert!(
        v.notebook.is_some(),
        "the pane must still be a notebook, not half-turned into a text view"
    );

    // And a fresh read went out — the only thing that can rebuild the cells.
    let sent = rx.try_recv().expect("a re-read of the changed notebook");
    assert!(
        matches!(&sent.request, clew_protocol::Request::ReadFile { rel, .. } if rel == "analysis.ipynb"),
        "expected a ReadFile for the notebook, got {:?}",
        sent.request
    );
    let _ = app.handle_server_reply(
        sent.id,
        clew_protocol::Event::NotebookContent {
            rel: "analysis.ipynb".into(),
            language: "python".into(),
            cells: vec![code_cell("second()")],
            symbols: Vec::new(),
            projection: "# %%\nsecond()\n".into(),
        },
    );
    let v = app.panes[0].as_ref().expect("the notebook pane");
    assert_eq!(
        v.notebook.as_ref().unwrap().cells[0].source,
        "second()",
        "the pane must end up showing the post-save cells"
    );
    assert!(
        v.source.contains("second()"),
        "and its projection must move with them: {}",
        v.source
    );
}

/// A notebook pane's outline entries are lines of the `# %%` script
/// projection, but `git log -L` resolves its range against the raw .ipynb JSON
/// on disk. Handing it a projection range yields a confident, entirely
/// unrelated commit list labelled with the cell's name, so a symbol scope must
/// be refused (and said out loud) rather than answered wrongly.
#[test]
fn time_travel_refuses_a_cell_scope_on_a_notebook() {
    let mut app = scanned_app("nb-time-travel");
    let root = app.project.as_ref().unwrap().root.clone();
    let abs = root.join("analysis.ipynb");
    std::fs::write(&abs, "{\"cells\":[{\"source\":\"load()\"}]}\n").unwrap();
    // The outline the server builds for a notebook: projection lines.
    let cell = Symbol {
        name: "[1] load()".into(),
        kind: "cell".into(),
        line: 2,
        end_line: 3,
    };
    let _ = app.apply_notebook_content(
        &[0],
        None,
        "analysis.ipynb".into(),
        "python".into(),
        vec![clew_protocol::NotebookCell {
            kind: "code".into(),
            source: "load()".into(),
            lines: Vec::new(),
            proj_line: 2,
            outputs: Vec::new(),
            execution_count: None,
        }],
        vec![cell.clone()],
        "# %%\nload()\n".into(),
        false,
    );
    // Pointing at that cell is ordinary: an Outline click sets the caret.
    app.panes[0].as_mut().unwrap().caret = Some((1, 0));

    let (scope, refused) = app.time_travel_scope(true);
    assert!(
        matches!(scope, TimeScope::File),
        "a projection range must never reach `git log -L` against the raw JSON"
    );
    assert!(refused, "and the reader must be told why the scope held");

    // Not vacuous: the very same caret and symbols on a plain-text pane — the
    // only difference being that the pane is not a notebook — do scope.
    app.panes[0].as_mut().unwrap().notebook = None;
    let (scope, refused) = app.time_travel_scope(true);
    assert!(
        matches!(
            scope,
            TimeScope::Symbol {
                start: 2,
                end: 3,
                ..
            }
        ),
        "a normal file still gets symbol-scoped history"
    );
    assert!(!refused);
}

/// A split shows the same notebook twice, and the watcher sends exactly ONE
/// re-read per changed file. If the reply rebuilds only the first pane that
/// matches, the other half keeps painting the pre-edit cells for the rest of
/// the session — nothing else ever rebuilds `v.notebook`, and the pane that
/// loses is the one the split just made active.
#[test]
fn a_changed_notebook_is_rebuilt_in_every_pane_showing_it() {
    let mut app = scanned_app("nb-split-refresh");
    let root = app.project.as_ref().unwrap().root.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);

    let abs = root.join("analysis.ipynb");
    let raw = |call: &str| format!("{{\"cells\":[{{\"source\":\"{call}\"}}]}}\n");
    std::fs::write(&abs, raw("first()")).unwrap();
    let code_cell = |call: &str| clew_protocol::NotebookCell {
        kind: "code".into(),
        source: call.to_string(),
        lines: Vec::new(),
        proj_line: 2,
        outputs: Vec::new(),
        execution_count: None,
    };
    let _ = app.apply_notebook_content(
        &[0],
        None,
        "analysis.ipynb".into(),
        "python".into(),
        vec![code_cell("first()")],
        Vec::new(),
        "# %%\nfirst()\n".into(),
        false,
    );
    // Side by side: pane 1 is a clone of pane 0 and becomes the active one.
    let _ = app.update(Message::ToggleSplit);
    assert_eq!(app.active, 1, "the split makes the new pane active");
    // Each half is read at its own place; a refresh must not level them.
    app.panes[1].as_mut().unwrap().scroll_y = 250.0;
    while rx.try_recv().is_ok() {}

    // Jupyter saves the notebook; one re-read goes out for it.
    let edited = raw("second()");
    std::fs::write(&abs, &edited).unwrap();
    let _ = app.update(rehashed(&app, vec![modified(&abs, 4242, &edited)]));
    let sent = rx.try_recv().expect("a re-read of the changed notebook");
    assert!(
        rx.try_recv().is_err(),
        "one request per changed file — the reply is what must fan out"
    );
    let _ = app.handle_server_reply(
        sent.id,
        clew_protocol::Event::NotebookContent {
            rel: "analysis.ipynb".into(),
            language: "python".into(),
            cells: vec![code_cell("second()")],
            symbols: Vec::new(),
            projection: "# %%\nsecond()\n".into(),
        },
    );

    for pane in 0..2 {
        let v = app.panes[pane].as_ref().expect("the notebook pane");
        assert_eq!(
            v.notebook.as_ref().unwrap().cells[0].source,
            "second()",
            "pane {pane} must show the post-save cells"
        );
        assert!(
            v.source.contains("second()"),
            "pane {pane}'s projection must move with them: {}",
            v.source
        );
    }
    assert_eq!(
        app.panes[1].as_ref().unwrap().scroll_y,
        250.0,
        "a refresh keeps each pane where its reader was, not where pane 0 was"
    );
}

#[test]
fn the_initial_index_does_not_strand_a_change_already_being_read() {
    // The registry is empty for the whole initial-index window, so a change
    // dispatched during it is baselined at 0. If the index result then fills
    // that slot with its own (older) read, the arriving batch reads as
    // superseded and the pre-edit bytes win: the seed and the change dispatch
    // pointing their ordering rules in opposite directions over one entry.
    let root = fixture_project("index-window-race");
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    let files = app.project.as_ref().unwrap().files.clone();
    let indexed = index::build_indexed(&root, files);
    let abs = root.join("src/lib.rs");

    // The watcher dispatches a read of that file while the build is still out.
    let _ = app.update(Message::FilesChanged(vec![abs.clone()]));
    assert!(app.registry.is_reading(&abs));

    // The build lands first, seeding everything it read.
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed,
    });

    // Then the read it overlapped with, carrying the baseline `0` that
    // `on_files_changed` recorded when the registry was still empty.
    let edited = "pub fn edited_during_index() {}\n";
    let hash = incremental::content_hash(edited.as_bytes());
    let _ = app.update(Message::FilesRehashed {
        root: root.clone(),
        epoch: app.project_epoch,
        events: vec![modified(&abs, hash, edited)],
        baselines: HashMap::from([(abs.clone(), 0)]),
        fs_structural: false,
    });

    assert_eq!(
        app.registry.version(&abs),
        Some(hash),
        "the read dispatched during the window is the newer one and must win"
    );
    assert!(
        app.symbol_index
            .iter()
            .any(|e| e.name == "edited_during_index")
    );
    assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
}

#[test]
fn tree_update_swaps_files_and_ignores_stale_root() {
    let mut app = scanned_app("tree");
    let root = app.project.as_ref().unwrap().root.clone();
    let before = app.project.as_ref().unwrap().files.len();

    // A new file on disk, applied via a rescan result, grows the file list
    // without a full project reopen.
    std::fs::write(root.join("src/newmod.rs"), "pub fn brand_new() {}\n").unwrap();
    let _ = app.update(Message::TreeUpdated {
        epoch: app.project_epoch,
        result: fs_scan::scan(root.clone()),
    });
    let after = app.project.as_ref().unwrap().files.len();
    assert_eq!(after, before + 1);
    assert!(
        app.project
            .as_ref()
            .unwrap()
            .files
            .iter()
            .any(|f| f.rel.ends_with("newmod.rs"))
    );

    // A rescan for a different root (a stale one) is ignored.
    let stale = fs_scan::ScanResult {
        root: PathBuf::from("/definitely/not/this/project"),
        tree: fs_scan::DirNode::default(),
        files: Vec::new(),
        truncated: false,
    };
    let _ = app.update(Message::TreeUpdated {
        epoch: app.project_epoch,
        result: stale,
    });
    assert_eq!(app.project.as_ref().unwrap().files.len(), after);

    // And so is a rescan of THIS root started under a previous project
    // instance — the root alone cannot tell two projects apart when they
    // share an absolute path on different hosts.
    let superseded = fs_scan::scan(root.clone());
    let _ = app.update(Message::TreeUpdated {
        epoch: app.project_epoch - 1,
        result: superseded,
    });
    assert_eq!(app.project.as_ref().unwrap().files.len(), after);
}

#[test]
fn symbol_finder_flow() {
    let mut app = scanned_app("symbols");
    // Build the index synchronously (the runtime does this in a task).
    let files = app.project.as_ref().unwrap().files.clone();
    let root = app.project.as_ref().unwrap().root.clone();
    let _ = app.update(Message::SymbolIndexDone {
        root: root.clone(),
        epoch: app.project_epoch,
        indexed: index::build_indexed(&root, files),
    });
    assert!(!app.indexing);
    assert!(app.symbol_index.len() >= 2, "{:?}", app.symbol_index);

    let _ = app.update(Message::FinderOpened(FinderMode::Symbols));
    let _ = app.update(Message::FinderQueryChanged("origin".to_string()));
    assert!(!app.finder.results.is_empty());
    let entry = &app.symbol_index[app.finder.results[0]];
    assert_eq!(entry.name, "origin");
    assert_eq!(entry.line, 3);

    // Confirm records the jump in history.
    let _ = app.update(Message::FinderConfirm);
    assert!(!app.finder.open);
    let _ = app.update(Message::GoBack); // no-op or previous loc; must not panic
}

#[test]
fn goto_line_via_finder() {
    let mut app = scanned_app("goto");
    open_synchronously(&mut app, "src/lib.rs", None);

    let _ = app.update(Message::GotoLineRequested);
    assert!(app.finder.open);
    let _ = app.update(Message::FinderQueryChanged(":4".to_string()));
    assert_eq!(app.finder.goto_line(), Some(4));
    let _ = app.update(Message::FinderConfirm);
    assert!(!app.finder.open);
    assert_eq!(app.active_viewer().unwrap().target_line, Some(4));
}

#[test]
fn split_view_routes_to_active_pane() {
    let mut app = scanned_app("split");
    open_synchronously(&mut app, "src/lib.rs", None);

    let _ = app.update(Message::ToggleSplit);
    assert!(app.split);
    assert_eq!(app.active, 1);
    // Split duplicates the current file.
    assert_eq!(app.panes[1].as_ref().unwrap().rel, "src/lib.rs");

    // Opening now targets pane 1; pane 0 keeps its file.
    open_synchronously(&mut app, "notes.txt", None);
    assert_eq!(app.panes[1].as_ref().unwrap().rel, "notes.txt");
    assert_eq!(app.panes[0].as_ref().unwrap().rel, "src/lib.rs");

    // Refocus pane 0 and close the split.
    let _ = app.update(Message::PaneFocused(0));
    assert_eq!(app.active, 0);
    let _ = app.update(Message::ToggleSplit);
    assert!(!app.split);
    assert!(app.panes[1].is_none());
}

#[test]
fn selection_and_copy_state() {
    let mut app = scanned_app("select");
    open_synchronously(&mut app, "src/lib.rs", None);

    let _ = app.update(Message::SelectStart {
        pane: 0,
        line: 1,
        col: 4,
    });
    assert!(app.selecting);
    assert_eq!(app.active_viewer().unwrap().caret, Some((1, 4)));
    let _ = app.update(Message::SelectDrag {
        pane: 0,
        line: 3,
        col: 2,
    });
    let _ = app.update(Message::SelectEnd);
    assert!(!app.selecting);

    let v = app.active_viewer().unwrap();
    assert_eq!(v.selection_ordered(), Some(((1, 4), (3, 2))));
    assert_eq!(v.caret, Some((3, 2)));
    let text = v.selected_text().unwrap();
    assert!(text.contains("origin"), "{text}");

    // Esc clears the selection.
    let _ = app.update(Message::KeyPressed(
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::default(),
    ));
    assert!(app.active_viewer().unwrap().selection.is_none());
}

#[test]
fn bookmark_toggle_persists_in_project() {
    let mut app = scanned_app("bookmark");
    let root = app.project.as_ref().unwrap().root.clone();
    open_synchronously(&mut app, "src/lib.rs", Some(3));

    let _ = app.update(Message::BookmarkToggled);
    assert_eq!(app.bookmarks.len(), 1);
    assert_eq!(app.bookmarks[0].rel, "src/lib.rs");
    assert_eq!(app.bookmarks[0].line, 3);
    assert!(root.join(".clew/bookmarks.json").exists());
    assert_eq!(bookmarks::load(&root), app.bookmarks);

    // Toggling again removes it and cleans up the store file; the .clew
    // directory itself stays (consent record).
    let _ = app.update(Message::BookmarkToggled);
    assert!(app.bookmarks.is_empty());
    assert!(!root.join(".clew/bookmarks.json").exists());
}

#[test]
fn consent_gates_project_open() {
    let data = std::env::temp_dir().join("clew-consent-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // Consent is recorded in clew's data directory, never in the project — a
    // repository must not be able to grant itself permission.
    let _env = data_dir_override(&data);

    let root = fixture_project("consent");

    // Picking an untrusted folder opens the consent modal, not the project.
    let mut app = blank_app();
    let _ = app.update(Message::FolderPicked(Some(root.clone())));
    assert_eq!(app.pending_consent.as_deref(), Some(root.as_path()));
    assert!(app.project.is_none() && !app.scanning);

    // Denied: no project opens, modal dismissed, nothing recorded.
    let _ = app.update(Message::ConsentDenied);
    assert!(app.pending_consent.is_none());
    assert!(app.project.is_none() && !app.scanning);
    assert!(app.status.contains("not allowed"), "{}", app.status);
    assert!(!clew_core::trust::Trust::load().is_root_trusted(None, &root));

    // A `.clew/` directory in the project does NOT imply consent: it ships with
    // the repository, so it would let a hostile project trust itself.
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    let mut planted = blank_app();
    let _ = planted.update(Message::FolderPicked(Some(root.clone())));
    assert_eq!(
        planted.pending_consent.as_deref(),
        Some(root.as_path()),
        "a repo-provided .clew must not grant consent"
    );
    assert!(!planted.scanning);

    // Allowed: the scan starts and the trust record is written outside the project.
    let mut app = blank_app();
    let _ = app.update(Message::FolderPicked(Some(root.clone())));
    let _ = app.update(Message::ConsentAllowed);
    assert!(app.scanning);
    assert!(app.pending_consent.is_none());
    assert!(clew_core::trust::Trust::load().is_root_trusted(None, &root));

    // A trusted root skips the modal on the next open.
    let mut app2 = blank_app();
    let _ = app2.update(Message::FolderPicked(Some(root.clone())));
    assert!(app2.scanning, "a trusted root must skip the prompt");
    assert!(app2.pending_consent.is_none());
}

#[test]
fn auto_refresh_throttles_but_manual_does_not() {
    use std::time::{Duration, Instant};

    let mut app = blank_app();
    app.llm_available = true;

    // Nothing explained yet → auto-refresh is a no-op (first build is manual).
    let _ = app.request_auto_refresh();
    assert!(app.last_auto_refresh.is_none() && !app.refresh_pending);

    // Seed one explanation so there's something to keep fresh.
    app.explain.cache.insert(
        explain::Node::File(PathBuf::from("a.rs")),
        explain::Cached {
            summary: "s".into(),
            prompt_hash: 1,
            detail: None,
        },
    );

    // First change fires immediately (no prior refresh): stamps the cooldown.
    let _ = app.request_auto_refresh();
    let first = app.last_auto_refresh.expect("cooldown stamped");
    assert!(!app.refresh_pending, "a fresh pass isn't 'pending'");

    // A second change inside the 30s window is deferred, not fired: the stamp
    // is unchanged and the pass is now pending.
    let _ = app.request_auto_refresh();
    assert_eq!(app.last_auto_refresh, Some(first), "cooldown not restamped");
    assert!(app.refresh_pending, "change during cooldown is queued");

    // Once the window has passed, the queued change fires and restamps.
    app.last_auto_refresh = Some(Instant::now() - Duration::from_secs(31));
    let _ = app.request_auto_refresh();
    assert!(
        app.last_auto_refresh.unwrap() > first,
        "restamped after cooldown"
    );
    assert!(!app.refresh_pending, "queued pass consumed");

    // A manual refresh ignores the cooldown entirely: fresh stamp even though
    // one was just set microseconds ago.
    let before = app.last_auto_refresh.unwrap();
    let _ = app.update(Message::RefreshAll);
    assert!(
        app.last_auto_refresh.unwrap() >= before,
        "manual bypasses cooldown"
    );
    assert!(!app.refresh_pending);
}

#[test]
fn search_flow_message_wiring() {
    let mut app = scanned_app("search");

    let files = app.project.as_ref().unwrap().files.clone();
    let result = search::search(
        files,
        search::SearchOptions {
            query: "needle".to_string(),
            ..Default::default()
        },
    );
    assert_eq!(result.hits.len(), 1);
    let _ = app.update(Message::SearchDone {
        seq: app.search_seq,
        result,
    });
    assert_eq!(app.search.hits.len(), 1);
    assert_eq!(app.search.hits[0].rel, "notes.txt");

    // Clicking a hit opens the file at that line.
    let hit = app.search.hits[0].clone();
    let _ = app.update(Message::OpenAbs {
        abs: hit.abs.clone(),
        line: Some(hit.line),
        push: true,
    });
    let content = read_text_file(&hit.abs).unwrap();
    let req = app.pane_pending[0].expect("open_file minted a load token");
    let _ = app.update(Message::FileLoaded {
        req,
        pane: 0,
        abs: hit.abs,
        target: Some(hit.line),
        result: Ok(content),
    });
    assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
}

#[test]
fn font_size_rescales_scroll() {
    let mut app = scanned_app("font");
    open_synchronously(&mut app, "src/lib.rs", None);
    app.panes[0].as_mut().unwrap().scroll_y = 40.0; // line 2 at 20px
    let _ = app.update(Message::FontSizeDelta(2.0));
    assert_eq!(app.font_size, 15.0);
    let v = app.active_viewer().unwrap();
    assert!((v.scroll_y - 44.0).abs() < 0.01, "{}", v.scroll_y); // 2 * 22px
    let _ = app.update(Message::FontSizeReset);
    assert_eq!(app.font_size, DEFAULT_FONT_SIZE);
}

#[test]
fn binary_and_oversized_files_are_rejected() {
    let dir = std::env::temp_dir().join("clew-guard-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("blob.bin");
    std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
    assert!(read_text_file(&bin).unwrap_err().contains("binary"));

    let big = dir.join("huge.txt");
    std::fs::write(&big, vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
    assert!(read_text_file(&big).unwrap_err().contains("too large"));
}

// ---------------------------------------------------------------- LSP

/// Opening a Rust file with no installed server and no override prompts
/// for a download; dismissing marks the language unsupported (falls back
/// to ⌘T).
#[test]
fn opening_rust_prompts_for_server_download() {
    // Point the store at a guaranteed-empty dir so nothing is "installed".
    let store = std::env::temp_dir().join("clew-lsp-empty-store");
    let _ = std::fs::remove_dir_all(&store);
    let _env = data_dir_override(&store);

    let mut app = scanned_app("lsp-prompt");
    open_synchronously(&mut app, "src/lib.rs", None);

    let consent = app.pending_lsp_consent.as_ref().expect("download prompt");
    assert_eq!(consent.server_name, "rust-analyzer");
    assert!(matches!(
        app.lsp.get("rust"),
        Some(LspSlot::AwaitingConsent)
    ));

    let _ = app.update(Message::LspConsentDismissed);
    assert!(app.pending_lsp_consent.is_none());
    assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Unsupported(_))));
}

/// The server panel lists only project-relevant languages — a Rust
/// project does not show c/cpp.
#[test]
fn managed_languages_are_project_relevant() {
    let app = scanned_app("lsp-langs"); // fixture has src/lib.rs (Rust) + notes.txt
    assert_eq!(app.managed_languages(), vec!["rust".to_string()]);
    // notes.txt has no server; c/cpp are not in the project.
    assert!(!app.managed_languages().iter().any(|l| l == "cpp"));
}

/// Right-click opens a navigation menu carrying the clicked position;
/// choosing an action closes it.
#[test]
fn context_menu_flow() {
    let mut app = scanned_app("ctxmenu");
    open_synchronously(&mut app, "src/lib.rs", None);

    let _ = app.update(Message::ContextMenuOpened {
        pane: 0,
        line: 2,
        col: 7,
        x: 120.0,
        y: 40.0,
    });
    let menu = app.context_menu.expect("menu open");
    assert_eq!((menu.line, menu.col), (2, 7));

    // Choosing an action closes the menu (and dispatches a goto).
    let _ = app.update(Message::ContextGoto(GotoKind::Definition));
    assert!(app.context_menu.is_none());

    // Outside click / Esc closes without acting.
    let _ = app.update(Message::ContextMenuOpened {
        pane: 0,
        line: 0,
        col: 0,
        x: 0.0,
        y: 0.0,
    });
    let _ = app.update(Message::ContextMenuClosed);
    assert!(app.context_menu.is_none());
}

/// A Go project surfaces the gopls row (toolchain-installed server).
#[test]
fn go_project_is_served_by_gopls() {
    let dir = std::env::temp_dir().join("clew-go-proj");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.go"), "package main\nfunc main() {}\n").unwrap();
    let root = dir.canonicalize().unwrap();

    let mut app = blank_app();
    scan_synchronously(&mut app, root);
    assert!(app.managed_languages().contains(&"go".to_string()));
    assert_eq!(
        lsp::registry::default_for_language("go").unwrap().name,
        "gopls"
    );
}

/// A `command` in the project's own lsp.toml must not run silently: the file
/// ships with the repository, so a hostile one could otherwise execute anything
/// as soon as a matching file is opened.
#[test]
fn custom_command_requires_approval() {
    let root = fixture_project("lsp-escape");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    // A real (readable) script: the fingerprint hashes its bytes.
    std::fs::write(root.join("run-lsp.sh"), "#!/bin/sh\nexec rust-analyzer\n").unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"run-lsp.sh\"\n",
    )
    .unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);

    // Nothing started; the user is asked, and sees the exact command line.
    assert!(!matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));
    let pending = app
        .pending_lsp_command
        .as_ref()
        .expect("a repo-specified command must be confirmed");
    assert!(
        pending
            .command_line()
            .is_some_and(|l| l.contains("run-lsp.sh"))
    );
    assert_eq!(pending.language, "rust");
    assert_eq!(pending.root, root, "the modal is bound to its project");

    // Declining leaves it unstarted.
    let _ = app.update(Message::LspCommandDismissed);
    assert!(app.pending_lsp_command.is_none());
    assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Unsupported(_))));
}

/// A command that can't be read can't be fingerprinted — it fails closed
/// instead of raising an approval modal for something unverifiable.
#[test]
fn unreadable_custom_command_fails_closed() {
    let root = fixture_project("lsp-unreadable");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"/nonexistent/rust-analyzer\"\n",
    )
    .unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root);
    open_synchronously(&mut app, "src/lib.rs", None);

    assert!(app.pending_lsp_command.is_none(), "nothing to approve");
    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::Failed(_))),
        "unreadable command must fail closed, got {:?}",
        std::mem::discriminant(app.lsp.get("rust").unwrap())
    );
}

/// An approval modal left open across a project switch must be void: it was
/// raised for the OLD project's command, and approving it must neither start
/// that command nor record anything for the new project.
#[test]
fn approval_modal_does_not_survive_a_project_switch() {
    let data = std::env::temp_dir().join("clew-lsp-switch-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);

    let root_a = fixture_project("lsp-switch-a");
    std::fs::create_dir_all(root_a.join(".clew")).unwrap();
    std::fs::write(root_a.join("run-lsp.sh"), "#!/bin/sh\nexec ra\n").unwrap();
    std::fs::write(
        root_a.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"run-lsp.sh\"\n",
    )
    .unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root_a.clone());
    open_synchronously(&mut app, "src/lib.rs", None);
    let fp = app
        .pending_lsp_command
        .as_ref()
        .expect("modal raised for project A")
        .fingerprint
        .clone();

    // The user switches projects with the modal still open.
    let root_b = fixture_project("lsp-switch-b");
    scan_synchronously(&mut app, root_b.clone());
    assert!(
        app.pending_lsp_command.is_none(),
        "the switch must void the old project's approval modal"
    );

    // Even a stale Allowed message (queued before the switch) is a no-op.
    app.pending_lsp_command = Some(PendingLspCommand {
        root: root_a.clone(),
        host: None,
        language: "rust".into(),
        command: Some(root_a.join("run-lsp.sh")),
        args: vec![],
        server_name: "rust-analyzer".into(),
        version: "x".into(),
        fingerprint: fp.clone(),
        init_options: None,
    });
    let _ = app.update(Message::LspCommandAllowed);
    assert!(
        !app.trust.is_lsp_approved(None, &root_a, "rust", &fp)
            && !app.trust.is_lsp_approved(None, &root_b, "rust", &fp),
        "approving a stale modal must record nothing"
    );
    assert!(!matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));
}

/// Approving the modal starts what the file contains NOW, not what it
/// contained when the modal was raised: a script swapped while the dialog sat
/// open fails the fresh fingerprint check and re-raises the modal instead of
/// running.
#[test]
fn approval_spawns_the_current_file_not_the_remembered_one() {
    let data = std::env::temp_dir().join("clew-lsp-toctou-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);

    let root = fixture_project("lsp-toctou");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    let script = root.join("run-lsp.sh");
    std::fs::write(&script, "#!/bin/sh\nexec ra\n").unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"run-lsp.sh\"\n",
    )
    .unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);
    let shown = app
        .pending_lsp_command
        .as_ref()
        .expect("modal raised")
        .fingerprint
        .clone();

    // The repository swaps the script's body while the dialog sits open.
    std::fs::write(&script, "#!/bin/sh\nexec ./payload\n").unwrap();

    let _ = app.update(Message::LspCommandAllowed);
    assert!(
        !matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "the swapped script must not start"
    );
    let re_raised = app
        .pending_lsp_command
        .as_ref()
        .expect("the changed command must be asked about again");
    assert_ne!(
        re_raised.fingerprint, shown,
        "the new modal shows the file as it is now"
    );
    // The recorded approval covers only what the user saw — the swapped
    // content is not approved.
    assert!(app.trust.is_lsp_approved(None, &root, "rust", &shown));
    assert!(
        !app.trust
            .is_lsp_approved(None, &root, "rust", &re_raised.fingerprint)
    );
}

/// Make the store believe `rust-analyzer` is already installed under `data`,
/// the way it would be after any earlier project provisioned it. Returns
/// nothing to spawn — the test never reaches an exec.
fn fake_installed_rust_analyzer(data: &Path) {
    let dir = data
        .join("servers")
        .join("rust-analyzer")
        .join(lsp::registry::by_name("rust-analyzer").unwrap().version);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rust-analyzer"), b"#!/bin/sh\nexit 0\n").unwrap();
}

/// `init_options` with NO `command` are the other half of what `.clew/lsp.toml`
/// decides, and they used to reach the server's `initialize` with no approval
/// of any kind: the binary came from the store, nothing was fingerprinted, and
/// the options went out verbatim. Cloning a repository, opening it and clicking
/// one file was then code execution — rust-analyzer runs
/// `cargo.buildScripts.overrideCommand` on workspace load.
#[test]
fn repo_init_options_without_a_command_require_approval() {
    // Fixture first: it primes the isolated data dir, which takes the same
    // non-reentrant env lock `data_dir_override` then holds (see `env_lock`).
    let root = fixture_project("lsp-options-only");
    let data = std::env::temp_dir().join("clew-lsp-options-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    fake_installed_rust_analyzer(&data);

    std::fs::create_dir_all(root.join(".clew")).unwrap();
    let hostile = "[rust.init_options]\n\
         \"rust-analyzer.cargo.buildScripts.overrideCommand\" = [\"/bin/sh\", \"-c\", \"id\"]\n";
    std::fs::write(root.join(".clew/lsp.toml"), hostile).unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);

    // Nothing started, and the user is asked — with the options in front of
    // them, since that is the whole of what they are approving.
    assert!(
        !matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "unapproved options must not reach initialize"
    );
    let pending = app
        .pending_lsp_command
        .as_ref()
        .expect("options with no command must be confirmed too");
    assert!(
        pending.command.is_none(),
        "no command was named — the modal must not claim one"
    );
    assert!(
        pending
            .init_options
            .as_deref()
            .is_some_and(|o| o.contains("overrideCommand") && o.contains("/bin/sh")),
        "the options being approved must be shown: {:?}",
        pending.init_options
    );
    let approved_fp = pending.fingerprint.clone();

    // Approving starts the server, and only then do the options apply.
    let _ = app.update(Message::LspCommandAllowed);
    assert!(app.trust.is_lsp_approved(None, &root, "rust", &approved_fp));
    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "an approved config must start"
    );

    // A later commit that edits only the options loses that approval.
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust.init_options]\n\"rust-analyzer.procMacro.server\" = \"./payload\"\n",
    )
    .unwrap();
    scan_synchronously(&mut app, root.clone());
    app.lsp.remove("rust");
    let _ = app.ensure_lsp("rust");
    let re_raised = app
        .pending_lsp_command
        .as_ref()
        .expect("edited options must be asked about again");
    assert_ne!(re_raised.fingerprint, approved_fp);
    assert!(
        !matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "the edited options must not start"
    );
}

/// The gate must fire for the options and nothing else: a project with no
/// `lsp.toml` (or one that sets no options) keeps starting its server without
/// a prompt. A gate that also caught the empty case would put an approval
/// dialog in front of every Rust project clew opens.
#[test]
fn a_config_without_options_still_starts_unprompted() {
    // Fixture first, for the env-lock ordering `data_dir_override` documents.
    let root = fixture_project("lsp-no-options");
    let data = std::env::temp_dir().join("clew-lsp-nooptions-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    fake_installed_rust_analyzer(&data);

    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);
    assert!(app.pending_lsp_command.is_none(), "nothing to approve");
    assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));

    // Same for a config that pins a version but sets no options.
    let root = fixture_project("lsp-no-options-2");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(root.join(".clew/lsp.toml"), "[rust]\nenabled = true\n").unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root);
    open_synchronously(&mut app, "src/lib.rs", None);
    assert!(app.pending_lsp_command.is_none(), "nothing to approve");
    assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));
}

/// One approval, one prompt — for the config that carries BOTH halves.
///
/// The options gate re-derives the command's fingerprint at start time instead
/// of trusting that `ensure_lsp` just staged it, because a rescan can swap
/// `lsp_config` in between. That means the value is computed in two places over
/// what must be the same inputs (`store::locate` hands back the config's own
/// `command`, so the staging and this re-derivation hash the same file). If
/// they ever drifted apart the user would approve, land back in `ensure_lsp`,
/// be asked again, and have no way out of the modal but closing the project —
/// so pin it: after Allow the server starts and nothing is left pending.
#[test]
fn an_approved_command_with_options_is_asked_about_once() {
    // Fixture first, for the env-lock ordering `data_dir_override` documents.
    let root = fixture_project("lsp-command-and-options");
    let data = std::env::temp_dir().join("clew-lsp-cmd-options-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);

    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(root.join("run-lsp.sh"), "#!/bin/sh\nexec rust-analyzer\n").unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"run-lsp.sh\"\n\n\
         [rust.init_options]\n\"rust-analyzer.check.command\" = \"clippy\"\n",
    )
    .unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);

    let pending = app
        .pending_lsp_command
        .as_ref()
        .expect("a repo-specified command must be confirmed");
    // The options ride inside the command's fingerprint, so they are part of
    // what is being approved and must be on screen with it.
    assert!(
        pending
            .init_options
            .as_deref()
            .is_some_and(|o| o.contains("clippy")),
        "the options folded into this approval must be shown: {:?}",
        pending.init_options
    );

    let _ = app.update(Message::LspCommandAllowed);
    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "the approved command must start, got {:?}",
        app.lsp.get("rust").map(std::mem::discriminant)
    );
    assert!(
        app.pending_lsp_command.is_none(),
        "one config, one question: the start must not re-raise the modal"
    );
}

/// The remote twin of `repo_init_options_without_a_command_require_approval`.
///
/// A remote `.clew/lsp.toml` that sets `init_options` and no `command` is the
/// ORDINARY shape of that file — the binary comes from clew's store, and the
/// project only tunes it. The server gates those options and cannot ask
/// anyone, so the grant has to happen here; when `Ready` carried no
/// fingerprint the client had no arm that could raise the modal, and such a
/// config was withheld on every open, across restarts and reconnects, with no
/// in-app way to allow it and no out-of-band one on a headless host.
///
/// A tokio test only because the final start proxies the server's stdio, which
/// spawns forwarding tasks; nothing here is awaited.
#[tokio::test]
async fn a_remote_options_only_config_is_approvable_from_the_client() {
    // Fixture first, for the env-lock ordering `data_dir_override` documents.
    let root = fixture_project("lsp-remote-options");
    let data = std::env::temp_dir().join("clew-lsp-remote-options-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);

    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);

    let hostile =
        "{\"rust-analyzer.cargo.buildScripts.overrideCommand\":[\"/bin/sh\",\"-c\",\"id\"]}";
    let root_s = root.to_string_lossy().into_owned();
    let resolved = |app: &mut App, resolution: clew_protocol::LspResolution| {
        app.lsp.insert("rust".into(), LspSlot::AwaitingConsent);
        let _ = app.update(Message::ServerEvent {
            conn: app.conn_gen,
            msg: clew_protocol::ServerMessage::Reply {
                id: 99,
                sub: None,
                event: clew_protocol::Event::LspResolved {
                    language: "rust".into(),
                    root: root_s.clone(),
                    resolution,
                },
            },
        });
    };
    let withheld = || clew_protocol::LspResolution::Ready {
        init_options: None,
        withheld: Some(clew_protocol::LspOptionsSpec {
            server: "rust-analyzer".into(),
            version: "x".into(),
            args: vec!["--stdio".into()],
            fingerprint: "fp-remote-options".into(),
            options: hostile.into(),
        }),
    };

    // The host withheld its options: the user is asked, with the options in
    // front of them, and nothing starts meanwhile.
    resolved(&mut app, withheld());
    let pending = app
        .pending_lsp_command
        .as_ref()
        .expect("withheld remote options must be offered for approval");
    assert!(
        pending.command.is_none(),
        "no command was named — the modal must not claim one"
    );
    assert_eq!(
        pending.host.as_deref(),
        Some("user@host"),
        "the approval is scoped to the host the config lives on"
    );
    assert_eq!(pending.root, root, "bound to the current project");
    assert_eq!(pending.fingerprint, "fp-remote-options");
    assert!(
        pending
            .init_options
            .as_deref()
            .is_some_and(|o| o.contains("overrideCommand") && o.contains("/bin/sh")),
        "the options being approved must be shown: {:?}",
        pending.init_options
    );
    assert!(
        !app.remote_lsp_init.contains_key("rust"),
        "withheld options must never be stashed for a start"
    );
    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::AwaitingConsent)),
        "the slot must keep waiting, or the reply to the re-resolve is dropped"
    );

    // Declining leaves them withheld: nothing recorded, nothing stashed.
    let _ = app.update(Message::LspCommandDismissed);
    assert!(
        !app.trust
            .is_lsp_approved(Some("user@host"), &root, "rust", "fp-remote-options"),
        "declining must record nothing"
    );
    assert!(!app.remote_lsp_init.contains_key("rust"));
    assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Unsupported(_))));

    // Allow: the approval is recorded, PUSHED to the host that enforces it,
    // and the resolve is re-issued so the host re-reads and re-fingerprints
    // its own lsp.toml — nothing runs on the copy the modal held.
    resolved(&mut app, withheld());
    while rx.try_recv().is_ok() {}
    let _ = app.update(Message::LspCommandAllowed);
    assert!(
        app.trust
            .is_lsp_approved(Some("user@host"), &root, "rust", "fp-remote-options"),
        "approving must record against the remote host"
    );
    let sent: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|m| m.request)
        .collect();
    assert!(
        sent.iter().any(|r| matches!(
            r,
            clew_protocol::Request::LspApprovals { approvals }
                if approvals.iter().any(|(l, f)| l == "rust" && f == "fp-remote-options")
        )),
        "the host enforces the gate — it must be told: {sent:?}"
    );
    assert!(
        sent.iter().any(
            |r| matches!(r, clew_protocol::Request::LspResolve { language } if language == "rust")
        ),
        "the allow must re-ask the host, not start from the modal's copy: {sent:?}"
    );

    // The host now answers with the options, and only THEY reach `initialize`.
    let _ = app.update(Message::ServerEvent {
        conn: app.conn_gen,
        msg: clew_protocol::ServerMessage::Reply {
            id: 100,
            sub: None,
            event: clew_protocol::Event::LspResolved {
                language: "rust".into(),
                root: root.to_string_lossy().into_owned(),
                resolution: clew_protocol::LspResolution::Ready {
                    init_options: Some(hostile.into()),
                    withheld: None,
                },
            },
        },
    });
    assert!(
        app.remote_lsp_init
            .get("rust")
            .is_some_and(|v| v.to_string().contains("overrideCommand")),
        "approved options must be handed to the handshake: {:?}",
        app.remote_lsp_init.get("rust")
    );
    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "an approved config must start, got {:?}",
        app.lsp.get("rust").map(std::mem::discriminant)
    );
}

/// A real `LspClient` whose transport is already gone: the stub peer completes
/// the `initialize` handshake and then hangs up, which is exactly what a
/// language server that dies mid-session leaves sitting in the slot.
async fn dead_lsp_client(root: &std::path::Path) -> lsp::client::LspClient {
    let (client_stdin, mut peer_rx) = tokio::io::duplex(64 * 1024);
    let (mut peer_tx, client_stdout) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // Answer only after the request is on the wire: replying earlier would
        // race the actor's own registration of the pending call.
        let mut buf = [0u8; 4096];
        let _ = peer_rx.read(&mut buf).await;
        // `initialize` is the client's first request, so it carries id 1.
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let _ = peer_tx.write_all(framed.as_bytes()).await;
        // Both halves drop here: the bytes stay readable, then EOF — the death.
    });
    let client = lsp::client::LspClient::connect(client_stdin, client_stdout, root, None)
        .await
        .expect("stub handshake");
    for _ in 0..200 {
        if !client.alive() {
            return client;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the peer hung up; the client should have seen EOF");
}

/// A language server that dies after startup must not stay `Ready`: its exit
/// clears the slot, so the next `ensure_lsp` starts a replacement instead of
/// handing out a client whose every request fails, and the counters and opened
/// documents the dead server owned are forgotten with it.
#[tokio::test]
async fn exited_lsp_process_clears_its_slot() {
    let mut app = scanned_app("lsp-exit");
    let file = app.project.as_ref().unwrap().root.join("src/lib.rs");
    let root = app.project.as_ref().unwrap().root.clone();
    app.lsp
        .insert("rust".into(), LspSlot::Ready(dead_lsp_client(&root).await));
    app.lsp_opened.insert(file.clone());
    app.lsp_procs.insert("rust".into(), 4);
    app.proc_feeds
        .insert(4, tokio::sync::mpsc::unbounded_channel().0);
    app.lsp_gen.insert("rust".into(), 1);
    app.seen_diag_version.insert("rust".into(), 7);
    app.seen_inlay_epoch.insert("rust".into(), 3);

    let _ = app.handle_server_event(clew_protocol::Event::ProcessExited {
        proc: 4,
        code: Some(101),
    });

    assert!(
        !app.lsp.contains_key("rust"),
        "a dead server must not keep a slot ensure_lsp short-circuits on"
    );
    assert!(app.proc_feeds.is_empty() && app.lsp_procs.is_empty());
    assert!(
        !app.lsp_opened.contains(&file),
        "documents must be re-opened against the replacement"
    );
    assert_eq!(
        app.lsp_gen.get("rust"),
        Some(&2),
        "any in-flight spawn is superseded"
    );
    assert!(!app.seen_diag_version.contains_key("rust"));
    assert!(!app.seen_inlay_epoch.contains_key("rust"));
    assert!(
        app.status.contains("rust") && app.status.contains("exited"),
        "the death is surfaced, not silent: {:?}",
        app.status
    );
}

/// The predecessor a restart killed exits late. Its mapping was dropped before
/// the kill, so its exit must leave the successor that replaced it untouched.
#[test]
fn killed_predecessor_exit_spares_the_successor() {
    let mut app = scanned_app("lsp-exit-predecessor");
    let file = app.project.as_ref().unwrap().root.join("src/lib.rs");
    app.lsp.insert("rust".into(), LspSlot::Starting);
    app.lsp_opened.insert(file.clone());
    app.lsp_procs.insert("rust".into(), 8); // the successor, already spawned
    app.lsp_gen.insert("rust".into(), 2);

    // Proc 7 is the killed predecessor: `start_lsp_with` removed its mapping.
    let _ = app.handle_server_event(clew_protocol::Event::ProcessExited {
        proc: 7,
        code: None,
    });

    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "the successor's slot must survive its predecessor's exit"
    );
    assert_eq!(app.lsp_procs.get("rust"), Some(&8));
    assert_eq!(
        app.lsp_gen.get("rust"),
        Some(&2),
        "the successor's own spawn must not be superseded"
    );
    assert!(app.lsp_opened.contains(&file));
}

/// The servers panel must not report a crashed server as "ready" — that lie is
/// what keeps the user from pressing Restart.
#[tokio::test]
async fn lsp_row_reports_a_stopped_server() {
    let mut app = scanned_app("lsp-row-dead");
    let root = app.project.as_ref().unwrap().root.clone();
    app.lsp
        .insert("rust".into(), LspSlot::Ready(dead_lsp_client(&root).await));

    let (status, action) = app.lsp_row("rust");
    assert!(status.contains("stopped"), "got {status:?}");
    assert!(
        matches!(action, Some(("Restart", _))),
        "a stopped server still offers Restart"
    );
}

/// A live `LspClient` whose stub peer answers the handshake and then keeps
/// listening, recording every byte the client sends — so the notifications
/// issued after startup are observable on the wire.
async fn recording_lsp_client(
    root: &std::path::Path,
) -> (lsp::client::LspClient, Arc<std::sync::Mutex<String>>) {
    let (client_stdin, mut peer_rx) = tokio::io::duplex(64 * 1024);
    let (mut peer_tx, client_stdout) = tokio::io::duplex(64 * 1024);
    let wire = Arc::new(std::sync::Mutex::new(String::new()));
    let recorder = wire.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut buf = vec![0u8; 64 * 1024];
        let mut answered = false;
        loop {
            let n = match peer_rx.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            // Header and body are written as separate frames, so decide on the
            // accumulated bytes rather than on one read. The guard is dropped
            // before the reply: it is not `Send`, so it may not cross an await.
            let handshake = {
                let mut seen = recorder.lock().unwrap();
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                seen.contains("\"method\":\"initialize\"")
            };
            // `initialize` is the client's first request, so it carries id 1.
            if !answered && handshake {
                answered = true;
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
                let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
                let _ = peer_tx.write_all(framed.as_bytes()).await;
            }
        }
    });
    let client = lsp::client::LspClient::connect(client_stdin, client_stdout, root, None)
        .await
        .expect("stub handshake");
    (client, wire)
}

/// Wait for `needle` to appear on the recorded wire and return everything sent
/// so far. Notifications are fire-and-forget through the client's actor, so
/// they land a scheduling hop after the call that issued them.
async fn wire_containing(wire: &Arc<std::sync::Mutex<String>>, needle: &str) -> String {
    for _ in 0..200 {
        let seen = wire.lock().unwrap().clone();
        if seen.contains(needle) {
            return seen;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "{needle:?} never reached the server; wire was {:?}",
        wire.lock().unwrap()
    );
}

/// A file edited on disk while it is OFF screen must still be resynced with the
/// language server. The server owns its copy of a document from `didOpen` until
/// a `didClose` clew never sends, and `open_docs_for_language` opens each path
/// exactly once — so with no `didChange` here the overlay stays at the pre-edit
/// text for the rest of the session, and definitions, references and inlay
/// hints touching that file resolve at shifted positions even after the pane
/// comes back to it.
#[tokio::test]
async fn an_off_screen_edit_still_reaches_the_language_server() {
    let mut app = scanned_app("lsp-offscreen-edit");
    let root = app.project.as_ref().unwrap().root.clone();
    let (client, wire) = recording_lsp_client(&root).await;
    app.lsp.insert("rust".into(), LspSlot::Ready(client));

    // src/lib.rs was opened earlier this session, so the server holds it; the
    // pane has since moved on to another file.
    let lib = root.join("src/lib.rs");
    app.lsp_opened.insert(lib.clone());
    open_synchronously(&mut app, "notes.txt", None);
    assert!(
        app.panes.iter().flatten().all(|v| v.abs != lib),
        "the edited file must be off screen for this to test anything"
    );

    let edited = "pub fn origin() {}\n\npub fn edited_off_screen() {}\n";
    std::fs::write(&lib, edited).unwrap();
    let _ = app.update(rehashed(&app, vec![modified(&lib, 4242, edited)]));

    let seen = wire_containing(&wire, "textDocument/didChange").await;
    assert!(
        seen.contains("edited_off_screen"),
        "the didChange must carry the post-edit text: {seen:?}"
    );
}

/// A definition result jumps to the target line and records history.
#[test]
fn definition_result_jumps_and_records_history() {
    let mut app = scanned_app("lsp-jump");
    open_synchronously(&mut app, "notes.txt", None);
    let target = app.project.as_ref().unwrap().root.join("src/lib.rs");

    let _ = app.update(Message::DefinitionResult {
        seq: app.goto_seq,
        result: Ok(vec![lsp::client::Target {
            path: target.clone(),
            line: 2, // 0-based → jump to line 3
            character: 7,
        }]),
    });
    // open_file kicked off an async load; feed the FileLoaded it awaits.
    let content = read_text_file(&target).unwrap();
    let req = app.pane_pending[0].expect("open_file minted a load token");
    let _ = app.update(Message::FileLoaded {
        req,
        pane: 0,
        abs: target,
        target: Some(3),
        result: Ok(content),
    });
    assert_eq!(app.active_viewer().unwrap().rel, "src/lib.rs");
    // The cursor moves to the jump target (line 3 → 0-based line 2).
    assert_eq!(app.active_viewer().unwrap().caret, Some((2, 0)));
    assert!(app.history.can_back(), "definition jump is undoable");
}

/// Full chain against a real rust-analyzer via the escape hatch: scan →
/// open → start server → didOpen → definition → jump. Ignored by default
/// (spawns rust-analyzer); run explicitly.
#[tokio::test]
#[ignore]
async fn live_goto_definition_through_app() {
    let ra = PathBuf::from(std::env::var("HOME").unwrap()).join(".cargo/bin/rust-analyzer");
    assert!(ra.exists(), "needs rust-analyzer at {ra:?}");

    // Cargo project with origin() defined and called.
    let root = std::env::temp_dir().join("clew-app-live-lsp");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        format!("[rust]\ncommand = {:?}\n", ra.to_string_lossy()),
    )
    .unwrap();
    let src = "fn origin() -> i32 {\n    0\n}\n\nfn main() {\n    let _ = origin();\n}\n";
    std::fs::write(root.join("src/main.rs"), src).unwrap();
    let root = root.canonicalize().unwrap();

    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/main.rs", None);

    // Start the real server (the escape hatch resolved it) and register it.
    let server = app.lsp_config.resolve("rust").unwrap();
    let client = lsp::client::LspClient::start(&server.command.unwrap(), &[], &root, None)
        .await
        .unwrap();
    app.lsp_gen.insert("rust".into(), 1);
    let _ = app.update(Message::LspStartResult {
        language: "rust".into(),
        generation: 1,
        result: Ok(client.clone()),
    });

    // Simulate ⌘-click on the `origin()` call (line 5, inside the name).
    let v = app.active_viewer().unwrap();
    let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
    let ch = viewer::character_offset(v.source_line(5).unwrap(), 12, utf16);
    let path = v.abs.clone();

    // Poll until rust-analyzer has indexed.
    let mut targets = Vec::new();
    for _ in 0..40 {
        targets = client.definition(&path, 5, ch).await.unwrap_or_default();
        if !targets.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(!targets.is_empty(), "expected a definition");

    // Feed the result through the app and complete the jump.
    let _ = app.update(Message::DefinitionResult {
        seq: app.goto_seq,
        result: Ok(targets.clone()),
    });
    let content = read_text_file(&targets[0].path).unwrap();
    let req = app.pane_pending[0].expect("open_file minted a load token");
    let _ = app.update(Message::FileLoaded {
        req,
        pane: 0,
        abs: targets[0].path.clone(),
        target: Some(targets[0].line + 1),
        result: Ok(content),
    });
    // Jumped to the `origin` definition on line 1 (1-based).
    assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
    assert!(app.history.can_back());
}

// ---- Staleness guards: late async results must not corrupt current state ---

/// A slower earlier open must not overwrite a faster later one: only the load
/// the pane still waits for may land.
#[test]
fn stale_file_load_does_not_overwrite_newer_open() {
    let mut app = scanned_app("stale-open");
    let root = app.project.as_ref().unwrap().root.clone();

    // Open A (slow — its FileLoaded will arrive last), then B.
    let _ = app.update(Message::OpenRel {
        rel: "src/lib.rs".into(),
        line: None,
    });
    let req_a = app.pane_pending[0].unwrap();
    let _ = app.update(Message::OpenRel {
        rel: "notes.txt".into(),
        line: None,
    });
    let req_b = app.pane_pending[0].unwrap();
    assert_ne!(req_a, req_b);

    // B's load lands first; the pane shows B.
    let _ = app.update(Message::FileLoaded {
        req: req_b,
        pane: 0,
        abs: root.join("notes.txt"),
        target: None,
        result: Ok(read_text_file(&root.join("notes.txt")).unwrap()),
    });
    assert_eq!(app.active_viewer().unwrap().rel, "notes.txt");

    // A's slower load arrives late: dropped, the pane still shows B.
    let _ = app.update(Message::FileLoaded {
        req: req_a,
        pane: 0,
        abs: root.join("src/lib.rs"),
        target: None,
        result: Ok(read_text_file(&root.join("src/lib.rs")).unwrap()),
    });
    assert_eq!(app.active_viewer().unwrap().rel, "notes.txt");
}

/// A load in flight for a project the user has already left must not land in
/// the new project (it used to join the old rel onto the new root).
#[test]
fn project_switch_voids_inflight_file_loads() {
    let mut app = scanned_app("stale-switch-a");
    let old_root = app.project.as_ref().unwrap().root.clone();
    let _ = app.update(Message::OpenRel {
        rel: "src/lib.rs".into(),
        line: None,
    });
    let req = app.pane_pending[0].expect("load in flight");

    // Switch projects while the load is still in flight.
    let root_b = fixture_project("stale-switch-b");
    scan_synchronously(&mut app, root_b);
    assert_eq!(app.pane_pending, [None, None], "switch clears load tokens");

    // The old project's load arrives: dropped, no pane appears.
    let _ = app.update(Message::FileLoaded {
        req,
        pane: 0,
        abs: old_root.join("src/lib.rs"),
        target: None,
        result: Ok("stale".into()),
    });
    assert!(
        app.panes[0].is_none(),
        "stale cross-project load must not open a pane"
    );
}

/// A slow scan of a project the user has already left must not re-open it.
#[test]
fn stale_scan_result_is_dropped() {
    let mut app = scanned_app("stale-scan-current");
    let current = app.project.as_ref().unwrap().root.clone();

    // A scan of another project finishes late (nothing pending anymore).
    let other = fixture_project("stale-scan-old");
    let _ = app.update(Message::ScanDone(fs_scan::scan(other)));
    assert_eq!(
        app.project.as_ref().unwrap().root,
        current,
        "a stale ScanDone must not switch the project"
    );
}

/// An LSP start result from a superseded spawn (restart bumped the generation)
/// must not install itself as the language's Ready client.
#[test]
fn stale_lsp_start_result_is_dropped() {
    let mut app = scanned_app("stale-lsp");
    app.lsp_gen.insert("rust".into(), 2);
    app.lsp.insert("rust".into(), LspSlot::Starting);
    let _ = app.update(Message::LspStartResult {
        language: "rust".into(),
        generation: 1, // superseded: current is 2
        result: Err("late failure from the old spawn".into()),
    });
    assert!(
        matches!(app.lsp.get("rust"), Some(LspSlot::Starting)),
        "a stale result must not touch the slot"
    );
}

/// Children fetched from a replaced call tree must not graft onto the new one
/// (node ids are bare indices).
#[test]
fn stale_call_children_do_not_graft_onto_new_tree() {
    let mut app = scanned_app("stale-calls");
    let item_path = app.project.as_ref().unwrap().root.join("src/lib.rs");
    let item = move |name: &str| lsp::client::CallItem {
        name: name.into(),
        detail: String::new(),
        kind: 12,
        path: item_path.clone(),
        line: 1,
        character: 0,
        raw: serde_json::json!({}),
    };
    // Install a tree the way the runtime does: a prepared request minted the
    // pending token, then the result installs the tree carrying it.
    app.call_token += 1;
    let old_token = app.call_token;
    app.call_pending = Some(old_token);
    let _ = app.update(Message::CallHierarchyPrepared {
        token: old_token,
        direction: callgraph::Direction::Incoming,
        lang: "rust",
        items: vec![item("root")],
    });
    assert_eq!(app.call_graph.as_ref().unwrap().token, old_token);

    // The tree is replaced (direction flip mints a new identity).
    let _ = app.update(Message::CallHierarchyDirection);
    let new_token = app.call_graph.as_ref().unwrap().token;
    assert_ne!(new_token, old_token);

    // Children from the old tree arrive: dropped.
    let _ = app.update(Message::CallHierarchyChildren {
        token: old_token,
        id: 0,
        items: vec![item("stale-child")],
    });
    let tree = app.call_graph.as_ref().unwrap();
    assert_eq!(tree.node_count(), 1, "stale children must not graft");

    // Children for the current tree still attach.
    let _ = app.update(Message::CallHierarchyChildren {
        token: new_token,
        id: 0,
        items: vec![item("fresh-child")],
    });
    assert_eq!(app.call_graph.as_ref().unwrap().node_count(), 2);
}

/// A search result from a superseded submission must not paint the sidebar.
#[test]
fn stale_search_result_is_dropped() {
    let mut app = scanned_app("stale-search");
    let old_seq = app.search_seq;
    app.search_seq += 1; // a newer submission happened
    app.search.running = true;
    let _ = app.update(Message::SearchDone {
        seq: old_seq,
        result: search::SearchResult {
            hits: vec![search::SearchHit {
                abs: app.project.as_ref().unwrap().root.join("notes.txt"),
                rel: "notes.txt".into(),
                line: 1,
                preview: "stale".into(),
            }],
            error: None,
        },
    });
    assert!(app.search.hits.is_empty(), "stale hits must not paint");
    assert!(
        app.search.running,
        "only the live submission may finish the spinner"
    );
}

/// A late event from a stopped debug run must not land on the next session.
#[test]
fn stale_debug_events_are_dropped() {
    let mut app = scanned_app("stale-debug");
    let session = || DebugSession {
        client: None,
        status: DebugStatus::Running,
        thread_id: None,
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: Vec::new(),
        current: None,
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        cwd: PathBuf::from("/"),
        port: None,
    };
    app.debug.session = Some(session());
    app.debug_run = 1;

    // The session is stopped (run identity ends), a new one starts.
    let _ = app.update(Message::DebugStop);
    app.debug.session = Some(session());
    let run = app.debug_run;

    // The old adapter's final Terminated drains late: dropped.
    let _ = app.update(Message::DapEvent {
        run: 1,
        event: dap::DapEvent::Terminated,
    });
    assert_eq!(
        app.debug.session.as_ref().unwrap().status,
        DebugStatus::Running,
        "a previous run's Terminated must not kill the new session"
    );
    // Old watch values are dropped too. The stop generation is the CURRENT
    // one, so the run mismatch alone is what has to reject this — otherwise
    // the assertion below would pass for the wrong reason.
    let _ = app.update(Message::DebugWatchesEvaluated {
        run: 1,
        stop: app.debug_stop,
        vals: vec![("x".into(), "stale".into())],
    });
    assert!(app.debug.session.as_ref().unwrap().watches.is_empty());
    // The current run's events still land.
    let _ = app.update(Message::DapEvent {
        run,
        event: dap::DapEvent::Terminated,
    });
    assert_eq!(
        app.debug.session.as_ref().unwrap().status,
        DebugStatus::Terminated
    );
}

/// A dead transport voids every in-flight request and stream, and bumps the
/// connection generation (which re-keys the subscription — the reconnect).
#[test]
fn server_disconnect_clears_inflight_state() {
    let mut app = scanned_app("disconnect");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.pending_reads.insert(
        7,
        ReadKind::Refresh {
            rel: "src/lib.rs".into(),
        },
    );
    app.pane_pending[0] = Some(7);
    app.pending_search = Some(9);
    app.proc_feeds
        .insert(1, tokio::sync::mpsc::unbounded_channel().0);
    app.lsp_procs.insert("rust".into(), 1);
    app.lsp_gen.insert("rust".into(), 3);
    let (otx, mut orx) = tokio::sync::oneshot::channel();
    app.ai_pending.lock().unwrap().insert(11, otx);
    let gen_before = app.conn_gen;

    let conn = app.conn_gen;
    let _ = app.update(Message::ServerDisconnected { conn });

    assert!(app.server_tx.is_none());
    assert_eq!(
        app.conn_gen,
        gen_before + 1,
        "reconnect via re-keyed subscription"
    );
    assert!(app.pending_reads.is_empty());
    assert_eq!(app.pane_pending, [None, None]);
    assert!(app.pending_search.is_none());
    assert!(app.proc_feeds.is_empty() && app.lsp_procs.is_empty());
    assert_eq!(
        app.lsp_gen.get("rust"),
        Some(&4),
        "in-flight spawns superseded"
    );
    // The awaiting AI task was woken with an error (sender dropped).
    assert!(orx.try_recv().is_err());
    assert!(app.ai_pending.lock().unwrap().is_empty());
}

/// A reconnect brings up a BRAND NEW clew-server, and everything the client
/// remembered about the old one's numbering is a lie: the fresh server's
/// publication counter restarts at 0, so keeping the old high-water mark
/// discarded its full snapshot and every partial until the new counter climbed
/// past it (and partials are never re-sent). Its `OpenProject` reply matters
/// for the same reason: the watcher restarts from the current state and reports
/// only later changes, so that reply is the only report of what happened while
/// the link was down. It resyncs the open project — it does not reopen it.
#[test]
fn reconnect_applies_the_new_servers_snapshot_and_file_list() {
    let root = fixture_project("reconnect-resync");
    let mut app = blank_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    let root_s = root.to_string_lossy().into_owned();
    let tree_reply = |app: &mut App, files: Vec<String>| {
        let _ = app.handle_server_reply(
            1,
            clew_protocol::Event::Tree {
                root: root.to_string_lossy().into_owned(),
                tree: clew_protocol::DirNode {
                    dirs: Vec::new(),
                    files: Vec::new(),
                },
                files,
                truncated: false,
            },
        );
    };
    let publish = |app: &mut App, seq: u64, symbol: &str| {
        let _ = app.handle_server_event(clew_protocol::Event::ProjectSymbols {
            root: root.to_string_lossy().into_owned(),
            seq,
            full: true,
            files: vec![clew_protocol::FileSymbols {
                rel: "src/lib.rs".into(),
                symbols: vec![clew_protocol::IndexSymbol {
                    name: symbol.into(),
                    kind: "function".into(),
                    line: 1,
                    is_test: false,
                }],
                imports: Vec::new(),
            }],
            go_module: clew_protocol::Patch::Unchanged,
            dart_package: clew_protocol::Patch::Unchanged,
            structure: clew_protocol::Patch::Unchanged,
        });
    };

    // Open through the server path, then let the first server publish a few
    // times so its counter is well above what a fresh one starts at.
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    tree_reply(&mut app, vec!["src/lib.rs".into()]);
    publish(&mut app, 5, "first_server_fn");
    assert!(app.symbol_index.iter().any(|s| s.name == "first_server_fn"));

    // The transport dies; the re-keyed subscription brings up a new server,
    // which greets us and is told about the project that is still open.
    let _ = app.update(Message::ServerDisconnected { conn: app.conn_gen });
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    let _ = app.update(Message::ServerConnected {
        conn: app.conn_gen,
        tx: tx2,
    });
    let _ = app.handle_server_reply(
        0,
        clew_protocol::Event::Ready {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        },
    );
    let mut reannounced = false;
    while let Ok(msg) = rx2.try_recv() {
        if let clew_protocol::Request::OpenProject { root } = &msg.request
            && *root == root_s
        {
            reannounced = true;
        }
    }
    assert!(reannounced, "the reconnect must re-announce the project");

    // Its Tree reply carries a file that appeared during the outage. It lands,
    // and the project is spliced rather than reopened — a reopen would close
    // every pane and drop the Ask history over a transport hiccup.
    let epoch = app.project_epoch;
    tree_reply(
        &mut app,
        vec!["src/lib.rs".into(), "src/added_while_down.rs".into()],
    );
    assert!(
        app.project
            .as_ref()
            .unwrap()
            .files
            .iter()
            .any(|f| f.rel == "src/added_while_down.rs"),
        "a file created during the outage must appear"
    );
    assert_eq!(app.project_epoch, epoch, "a resync is not a reopen");

    // And the new server's numbering starts over: its first snapshot must be
    // applied, not dismissed as older than the dead server's fifth.
    publish(&mut app, 1, "second_server_fn");
    assert!(
        app.symbol_index
            .iter()
            .any(|s| s.name == "second_server_fn"),
        "the fresh server's snapshot must apply: {:?}",
        app.symbol_index.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// A remote project whose transport is down must fail closed: no local
/// filesystem fallback for opens, searches, or deferred scans. The project's
/// absolute paths belong to the remote host — a same-pathed local file is a
/// different machine's data and must never be shown (or indexed) in its place.
#[test]
fn remote_disconnect_never_falls_back_to_local_files() {
    let mut app = scanned_app("remote-fail-closed");
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let conn = app.conn_gen;
    let _ = app.update(Message::ServerDisconnected { conn });
    assert!(app.server_tx.is_none());

    // Open: no local load token is minted, so no local read can land.
    let abs = app.project.as_ref().unwrap().root.join("src/lib.rs");
    let _ = app.open_file(abs, Some(1), true);
    assert_eq!(app.pane_pending, [None, None], "no local read scheduled");
    assert!(
        app.status.contains("Disconnected"),
        "status: {}",
        app.status
    );

    // Search: reports the disconnect instead of grepping this machine.
    app.search.query = "needle".into();
    let _ = app.run_search();
    assert!(!app.search.running);
    assert!(
        app.search
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Disconnected"),
        "search error: {:?}",
        app.search.error
    );

    // A deferred scan root is dropped, not scanned locally. Parked through
    // `start_scan` rather than assigned: that is what raises `scanning`, and
    // assigning the root alone left the flag false, so the assertion below
    // could not fail no matter what the handler did.
    let root = app.project.as_ref().unwrap().root.clone();
    let _ = app.start_scan(root);
    assert!(app.scanning, "start_scan raises the placeholder");
    assert!(app.pending_scan_root.is_some(), "the root is parked");
    let _ = app.update(Message::ServerUnavailable { conn: app.conn_gen });
    assert!(app.pending_scan_root.is_none());
    assert!(
        !app.scanning,
        "abandoning the parked root must take the Scanning placeholder down with it"
    );
}

/// The Connect modal's branch of `on_server_unavailable` drops the parked root
/// too, so it owes the same pairing: a bootstrap failure reported into the
/// modal must not leave `scanning` set behind the error, or dismissing the
/// modal reveals a window masked by "Scanning project…" with nothing left that
/// could ever answer it.
#[test]
fn connect_bootstrap_failure_clears_the_scanning_placeholder() {
    let mut app = scanned_app("connect-unavailable-scanning");
    let root = app.project.as_ref().unwrap().root.clone();
    app.server_tx = None;
    // No transport: `start_scan` parks the root and raises the placeholder.
    let _ = app.start_scan(root);
    assert!(app.scanning && app.pending_scan_root.is_some());
    app.connect = Some(ConnectUi {
        stage: ConnectStage::Connecting {
            label: "user@host".into(),
        },
        ..ConnectUi::default()
    });

    let _ = app.update(Message::ServerUnavailable { conn: app.conn_gen });

    assert!(
        matches!(
            app.connect.as_ref().map(|u| &u.stage),
            Some(ConnectStage::Error(_))
        ),
        "the failure belongs in the modal"
    );
    assert!(app.pending_scan_root.is_none());
    assert!(
        !app.scanning,
        "the abandoned scan must not leave the window masked"
    );
}

/// Park a scan the way the first open of a session does — no transport yet,
/// so `start_scan` defers — and bring a transport up under it. The returned
/// receiver must be held: dropping it closes the channel the Hello goes out on.
fn deferred_scan_with_transport(
    app: &mut App,
    root: &Path,
) -> tokio::sync::mpsc::UnboundedReceiver<clew_protocol::ClientMessage> {
    let _ = app.start_scan(root.to_path_buf());
    assert_eq!(
        app.pending_scan_root.as_deref(),
        Some(root),
        "the first open of a session must defer"
    );
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = app.update(Message::ServerConnected {
        conn: app.conn_gen,
        tx,
    });
    rx
}

/// A failed handshake must release the deferred scan. `start_scan` parks its
/// root waiting for a server and `on_server_ready` is the only code that ever
/// sends it, so a server built from different protocol sources used to leave
/// `scanning` and `pending_scan_root` set with no fallback and no retry — the
/// window sat on "Scanning…" with no project for the rest of the session, and
/// re-picking the folder only re-parked it.
#[test]
fn failed_handshake_falls_back_to_a_local_scan() {
    let root = fixture_project("handshake-mismatch");
    let mut app = blank_app();
    let _rx = deferred_scan_with_transport(&mut app, &root);

    // The server answers our Hello carrying another build's fingerprint.
    let task = app.handle_server_reply(
        crate::app::handlers_features::HELLO_REQ_ID,
        clew_protocol::Event::Ready {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: "0000000000000000".into(),
        },
    );
    assert!(
        app.server_tx.is_none(),
        "a transport whose protocol we don't share is dropped"
    );
    assert!(
        app.status.contains("different protocol sources"),
        "the status must name the mismatch: {}",
        app.status
    );
    // The escape hatch: the parked root went to the local scanner (which
    // re-parks it as the scan it is now waiting on), not nowhere.
    assert_eq!(task.units(), 1, "a local scan must have been scheduled");
    assert_eq!(app.pending_scan_root.as_ref(), Some(&root));
}

/// The refusal a live mismatch actually produces: the server checks the
/// client's version and fingerprint first, so it answers the Hello with an
/// `Error` rather than the `Ready` above — and then refuses every later
/// request too. That error correlates to no panel, so it used to do nothing
/// but set the status line while the scan stayed parked forever.
#[test]
fn refused_hello_falls_back_to_a_local_scan() {
    let root = fixture_project("handshake-refused");
    let mut app = blank_app();
    let _rx = deferred_scan_with_transport(&mut app, &root);

    let task = app.handle_server_reply(
        crate::app::handlers_features::HELLO_REQ_ID,
        clew_protocol::Event::Error {
            message: "protocol build mismatch: rebuild so they match".into(),
        },
    );
    assert!(app.server_tx.is_none());
    assert!(
        app.status.contains("build mismatch"),
        "the server's reason must reach the status bar: {}",
        app.status
    );
    assert_eq!(task.units(), 1, "a local scan must have been scheduled");
    assert_eq!(app.pending_scan_root.as_ref(), Some(&root));
}

/// A server that can NEVER succeed must not turn into an endless respawn
/// loop. The refusal drops the transport, the server exits on that EOF, and
/// the disconnect handler re-keyed the subscription — which spawned the same
/// incompatible binary again, every ~1.5 s for the rest of the session, each
/// cycle tearing down the language servers this client had started locally
/// (its own children) and burying the one message that says how to fix it.
/// A transport that merely DIED must still reconnect.
#[test]
fn a_permanently_incompatible_server_stops_respawning() {
    let root = fixture_project("handshake-no-retry");
    let mut app = blank_app();
    let rx = deferred_scan_with_transport(&mut app, &root);
    let conn = app.conn_gen;

    let _ = app.handle_server_reply(
        crate::app::handlers_features::HELLO_REQ_ID,
        clew_protocol::Event::Ready {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: "0000000000000000".into(),
        },
    );
    // Dropping the transport is what the server sees as EOF; it exits, and
    // the reader reports the death.
    drop(rx);
    let _ = app.update(Message::ServerDisconnected { conn });

    assert_eq!(
        app.conn_gen, conn,
        "re-keying the subscription IS the respawn — against this binary it \
         can only produce the same refusal"
    );
    assert!(
        app.status.contains("different protocol sources"),
        "the actionable reason must survive the disconnect, not be replaced by \
         a reconnect line: {}",
        app.status
    );

    // The window is not stranded: asking for a project re-arms exactly one
    // attempt (a rebuilt server is picked up there), so `start_scan` cannot
    // park a root waiting for a server that will never come.
    let _ = app.update(Message::FolderPicked(Some(root)));
    assert_eq!(
        app.conn_gen,
        conn + 1,
        "the user's open re-arms the transport"
    );
    assert!(app.handshake_failure.is_none());

    // And an ordinary transport death — a crashed server, a dropped SSH link
    // — still reconnects, which is the case the loop was written for.
    let (tx, _rx2) = tokio::sync::mpsc::unbounded_channel();
    let _ = app.update(Message::ServerConnected {
        conn: app.conn_gen,
        tx,
    });
    let live = app.conn_gen;
    let _ = app.update(Message::ServerDisconnected { conn: live });
    assert_eq!(app.conn_gen, live + 1, "a crash must still be retried");
    assert!(app.conn_respawn, "and retried on the crash backoff");
}

/// The same failure on a REMOTE connection takes the other half of the
/// fallback: the parked root names a path on the other host, so scanning it
/// here would open whatever this machine happens to have there. Release the
/// scan without one — but release it, rather than leaving the window stuck.
#[test]
fn failed_handshake_on_a_remote_never_scans_locally() {
    let mut app = blank_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let root = PathBuf::from("/srv/code/project");
    let _rx = deferred_scan_with_transport(&mut app, &root);

    let task = app.handle_server_reply(
        crate::app::handlers_features::HELLO_REQ_ID,
        clew_protocol::Event::Ready {
            protocol: clew_protocol::PROTOCOL_VERSION.wrapping_add(1),
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        },
    );
    assert_eq!(task.units(), 0, "no local scan of a remote root");
    assert!(
        app.pending_scan_root.is_none(),
        "the parked root is dropped"
    );
    assert!(!app.scanning, "the scanning placeholder must come down");
    assert!(
        app.status.contains("protocol"),
        "the status must explain why: {}",
        app.status
    );
}

/// Opening a different project must drop the Ask conversation and pinned
/// code: the next question replays recent turns and every pin to the
/// connected server, so keeping them would ship the previous project's
/// source (and conversation) to whatever host is now connected.
#[test]
fn project_switch_clears_ask_history_and_pins() {
    let mut app = scanned_app("ask-clear-a");
    app.ask_turns.push(AskTurn {
        stream: 1,
        question: "what does origin do?".into(),
        answer_md: "returns Point".into(),
        answer: Vec::new(),
        sources: Vec::new(),
        steps: Vec::new(),
        streaming: false,
    });
    app.ask_pins.push(AskPin {
        rel: "src/lib.rs".into(),
        file: app.project.as_ref().unwrap().root.join("src/lib.rs"),
        line: 1,
        code: "pub struct Point { x: f64 }".into(),
    });
    app.ask_input = "half-typed question".into();

    let other = fixture_project("ask-clear-b");
    scan_synchronously(&mut app, other);

    assert!(
        app.ask_turns.is_empty(),
        "turns must not survive the switch"
    );
    assert!(app.ask_pins.is_empty(), "pins must not survive the switch");
    assert!(app.ask_input.is_empty());
}

/// The remote filesystem boundary, end to end on the client: opening a
/// remote project must not read the same-pathed LOCAL `.clew/` state or run
/// the local indexer; the symbol index fills from the server's
/// `ProjectSymbols` snapshot instead; and saving state writes nothing to the
/// local disk. The fixture dir stands in for the remote path — everything in
/// it is "another machine's data".
#[test]
fn remote_project_never_touches_local_state_or_files() {
    let root = fixture_project("remote-isolation");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(
        root.join(".clew/bookmarks.json"),
        r#"[{"rel":"src/lib.rs","line":1,"preview":"local secret"}]"#,
    )
    .unwrap();

    let mut app = blank_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    // Open via the server path: the Tree reply builds the project.
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    let _ = app.handle_server_reply(
        1,
        clew_protocol::Event::Tree {
            root: root.to_string_lossy().into_owned(),
            tree: clew_protocol::DirNode {
                dirs: Vec::new(),
                files: Vec::new(),
            },
            files: vec!["src/lib.rs".into()],
            truncated: false,
        },
    );
    assert!(app.project.is_some());
    // The planted local .clew state was not read…
    assert!(
        app.bookmarks.is_empty(),
        "local bookmarks must not load for a remote project"
    );
    // …and the local indexer did not run.
    assert!(app.symbol_index_by_file.is_empty());
    assert!(app.indexing, "waiting on the server's snapshot");

    // The server's snapshot fills the index — a symbol the local file does
    // not contain proves the data came over the wire, not off this disk.
    let _ = app.handle_server_event(clew_protocol::Event::ProjectSymbols {
        root: root.to_string_lossy().into_owned(),
        seq: 1,
        full: true,
        files: vec![clew_protocol::FileSymbols {
            rel: "src/lib.rs".into(),
            symbols: vec![clew_protocol::IndexSymbol {
                name: "remote_only_fn".into(),
                kind: "function".into(),
                line: 3,
                is_test: false,
            }],
            imports: vec![clew_protocol::WireImport {
                module: "crate::helper".into(),
                line: 1,
                is_mod: false,
            }],
        }],
        go_module: clew_protocol::Patch::Set(None),
        dart_package: clew_protocol::Patch::Set(None),
        structure: clew_protocol::Patch::Set(None),
    });
    assert!(!app.indexing);
    assert!(app.symbol_index.iter().any(|s| s.name == "remote_only_fn"));
    assert!(
        !app.symbol_index.iter().any(|s| s.name == "origin"),
        "the same-pathed local file must not be indexed"
    );
    // The import graph built from the snapshot's extraction, without any
    // local read: the wire-carried specifier is in the graph.
    assert!(
        app.import_graph
            .imports(&root.join("src/lib.rs"))
            .iter()
            .any(|e| e.specifier == "crate::helper"),
        "the snapshot's imports must reach the graph"
    );

    // Session state loads from the REMOTE .clew via StateContent — never
    // from the planted local file.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root.to_string_lossy().into_owned(),
        rel: "bookmarks.json".into(),
        text: Some(r#"[{"rel":"src/lib.rs","line":2,"preview":"remote mark"}]"#.into()),
    });
    assert_eq!(app.bookmarks.len(), 1);
    assert_eq!(app.bookmarks[0].preview, "remote mark");

    // Saving goes over the protocol (a WriteState request), and nothing
    // lands in the local .clew.
    let drain = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<clew_protocol::ClientMessage>| {
        let mut writes = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let clew_protocol::Request::WriteState { root, rel, .. } = msg.request {
                writes.push((root, rel));
            }
        }
        writes
    };
    let _ = drain(&mut rx);

    // history.json has not loaded yet, so this client still holds the EMPTY
    // baseline for it. Writing that back would replace the remote file — and
    // an empty trail serializes to None, which DELETES it. The write is held.
    app.save_history();
    assert!(
        !root.join(".clew/history.json").exists(),
        "a remote project must not write local state files"
    );
    assert!(
        drain(&mut rx).is_empty(),
        "a save must not push an empty baseline over state that has not loaded"
    );

    // Once the real content arrives, the user's version is the one kept (the
    // load must not silently revert what they did) and it is written out.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root.to_string_lossy().into_owned(),
        rel: "history.json".into(),
        text: Some("{}".into()),
    });
    let writes = drain(&mut rx);
    assert!(
        writes
            .iter()
            .any(|(r, rel)| rel == "history.json" && r == &root.to_string_lossy()),
        "the held save must be flushed, naming its own project: {writes:?}"
    );
}

/// A remote project's bookmarks, notes and tours are edited by two clients at
/// once (two machines, or two windows — each window runs its own SSH session
/// and its own remote clew-server). A save that ships this window's whole list
/// therefore deleted everything the other client had written since this one
/// loaded the file, and there is no re-read except at project open and on
/// reconnect, so nothing looked wrong until then. The change must go out as an
/// entry-level merge the SERVER applies, and the merged file it replies with
/// must replace this window's copy.
#[test]
fn a_remote_bookmark_change_travels_as_a_merge_not_as_a_snapshot() {
    let root = fixture_project("remote-state-merge");
    let mut app = blank_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    let _ = app.handle_server_reply(
        1,
        clew_protocol::Event::Tree {
            root: root.to_string_lossy().into_owned(),
            tree: clew_protocol::DirNode {
                dirs: Vec::new(),
                files: Vec::new(),
            },
            files: vec!["src/lib.rs".into()],
            truncated: false,
        },
    );
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root.to_string_lossy().into_owned(),
        rel: "bookmarks.json".into(),
        text: Some(
            r#"[{"rel":"a.rs","line":1,"preview":"a"},{"rel":"b.rs","line":2,"preview":"b"}]"#
                .into(),
        ),
    });
    assert_eq!(app.bookmarks.len(), 2);

    // Delete the first one.
    let _ = app.on_bookmark_removed(0);
    let mut edit = None;
    let mut snapshots = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        match msg.request {
            clew_protocol::Request::EditState { rel, merge, .. } => {
                edit = Some((msg.id, rel, merge))
            }
            clew_protocol::Request::WriteState { rel, .. } => snapshots.push(rel),
            _ => {}
        }
    }
    assert!(
        !snapshots.iter().any(|rel| rel == "bookmarks.json"),
        "a whole-list write is what deleted the other client's bookmarks"
    );
    let (id, rel, merge) = edit.expect("the change went out as a merge");
    assert_eq!(rel, "bookmarks.json");
    assert!(
        matches!(merge.edit, clew_protocol::StateEdit::Remove),
        "and it names the entry by identity, not by this window's index"
    );
    assert_eq!(merge.key, vec![serde_json::json!("a.rs"), 1.into()]);

    // A read that was already in flight describes the file BEFORE that edit.
    // Adopting it would revert the deletion, and re-flushing this window's
    // list over it would undo the merge the edit exists to get.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root.to_string_lossy().into_owned(),
        rel: "bookmarks.json".into(),
        text: Some(
            r#"[{"rel":"a.rs","line":1,"preview":"a"},{"rel":"b.rs","line":2,"preview":"b"}]"#
                .into(),
        ),
    });
    assert_eq!(app.bookmarks.len(), 1, "the deletion holds");
    while let Ok(msg) = rx.try_recv() {
        if let clew_protocol::Request::WriteState { rel, .. } = msg.request {
            assert_ne!(rel, "bookmarks.json", "no snapshot may chase the merge");
        }
    }

    // The merged file the server replies with is the truth: it carries a
    // bookmark THIS window never had, and it is adopted rather than argued
    // with — sorted, since the merge appends where the store inserts in order.
    let _ = app.handle_server_reply(
        id,
        clew_protocol::Event::StateEdited {
            root: root.to_string_lossy().into_owned(),
            rel: "bookmarks.json".into(),
            text: Some(
                r#"[{"rel":"b.rs","line":2,"preview":"b"},{"rel":"a.rs","line":9,"preview":"theirs"}]"#.into(),
            ),
        },
    );
    let seen: Vec<_> = app
        .bookmarks
        .iter()
        .map(|b| (b.rel.as_str(), b.line))
        .collect();
    assert_eq!(seen, [("a.rs", 9), ("b.rs", 2)]);
    assert!(
        !app.remote_state_dirty.contains("bookmarks.json"),
        "the acknowledged change is no longer unsaved"
    );
}

/// A remote watcher notification must not make the client read files from
/// its OWN disk: a same-pathed local file is another machine's data. (The
/// local-connection path routes through the full derived-state pipeline
/// instead, which reads local files legitimately.)
#[test]
fn remote_files_changed_never_reads_the_local_disk() {
    let mut app = scanned_app("remote-watch");
    let root = app.project.as_ref().unwrap().root.clone();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);

    // A "remote" change whose path happens to exist locally too — the
    // classic same-absolute-path collision.
    let local_file = root.join("src/planted.rs");
    std::fs::write(&local_file, "pub fn local_secret() {}\n").unwrap();
    let _ = app.handle_server_event(clew_protocol::Event::FilesChanged {
        root: root.to_string_lossy().into_owned(),
        rels: vec!["src/planted.rs".into()],
    });
    assert!(
        !app.symbol_index_by_file.contains_key(&local_file),
        "a remote change must not be indexed from the local filesystem"
    );
    assert!(
        !app.symbol_index
            .iter()
            .any(|s| s.name.contains("local_secret")),
        "local file content leaked into the index of a remote project"
    );
}

/// A remote edit must invalidate the derived caches that key on the change
/// registry, whether or not the edited file is open, and must do it exactly
/// once. The client never sees a remote file's bytes, so the registry can
/// only learn of the change from the server's `ProjectSymbols` publication —
/// which used to refresh the symbol index while leaving the revision alone,
/// so Stats and Project Calls went on serving pre-edit results, visibly
/// disagreeing with the sidebar the same event stream had just updated. The
/// re-read that follows for an open file reports the SAME edit, so it must
/// not invalidate a second time. A source change also ages the explanations,
/// which no server event does for us.
#[test]
fn remote_symbol_update_advances_the_registry_and_ages_the_caches() {
    let mut app = scanned_app("remote-invalidate");
    let root = app.project.as_ref().unwrap().root.clone();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);

    // One publication of `src/lib.rs`, as the server sends it: `full` is the
    // snapshot built after the scan, a partial is the watcher reporting a
    // change to that one file.
    let publish = |app: &mut App, seq: u64, full: bool, line: usize| {
        let _ = app.handle_server_event(clew_protocol::Event::ProjectSymbols {
            root: root.to_string_lossy().into_owned(),
            seq,
            full,
            files: vec![clew_protocol::FileSymbols {
                rel: "src/lib.rs".into(),
                symbols: vec![clew_protocol::IndexSymbol {
                    name: "origin".into(),
                    kind: "function".into(),
                    line,
                    is_test: false,
                }],
                imports: Vec::new(),
            }],
            go_module: clew_protocol::Patch::Unchanged,
            dart_package: clew_protocol::Patch::Unchanged,
            structure: clew_protocol::Patch::Unchanged,
        });
    };

    publish(&mut app, 1, true, 3);
    // Both caches now hold a result computed at this revision, as they would
    // after the user visited Stats and the project call graph.
    app.stats.report = Some(stats::StatsReport::default());
    app.stats.rev = app.registry.revision();
    app.project_calls.rev = app.registry.revision();
    let seeded = app.registry.revision();

    // The edit lands in a file NO pane is showing, so the partial publication
    // is the only notice this client gets of it.
    publish(&mut app, 2, false, 9);
    assert!(
        app.symbol_index.iter().any(|s| s.line == 9),
        "the publication's symbols must reach the index"
    );
    assert_ne!(
        app.registry.revision(),
        seeded,
        "a remote change must advance the registry revision"
    );
    assert_ne!(
        app.stats.rev,
        app.registry.revision(),
        "Stats must read as stale after a remote change"
    );
    assert_ne!(
        app.project_calls.rev,
        app.registry.revision(),
        "the project call graph must read as stale after a remote change"
    );

    // The watcher notification for the same edit: an open file is re-read
    // from the server, and applying that reply describes the change the
    // publication already reported — one change, one invalidation.
    let changed = app.registry.revision();
    let _ = app.handle_server_event(clew_protocol::Event::FilesChanged {
        root: root.to_string_lossy().into_owned(),
        rels: vec!["src/lib.rs".into()],
    });
    let _ = app.apply_file_refresh(
        "src/lib.rs".into(),
        "pub fn origin() {}\n".into(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(
        app.registry.revision(),
        changed,
        "the re-read of an already-reported change must not invalidate again"
    );

    // And the same notification ages the understanding: the explain pass
    // fetches its sources over the protocol, so it is as reachable here as on
    // the local path — it just never used to be called.
    app.llm_available = true;
    app.explain.cache.insert(
        explain::Node::File(root.join("src/lib.rs")),
        explain::Cached {
            summary: "s".into(),
            prompt_hash: 1,
            detail: None,
        },
    );
    let _ = app.handle_server_event(clew_protocol::Event::FilesChanged {
        root: root.to_string_lossy().into_owned(),
        rels: vec!["src/lib.rs".into()],
    });
    assert!(
        app.last_auto_refresh.is_some(),
        "a remote source change must request the explanation refresh"
    );
}

/// Without the per-host opt-in, a remote connection must keep AI on the
/// client: endpoint Client (so no Chat/Embed RPC carries data to the host)
/// and no SetAiConfig (so no API key ever crosses the SSH link).
#[test]
fn remote_without_opt_in_keeps_ai_keys_on_the_client() {
    let mut app = scanned_app("remote-ai-gate");
    // Local: the server is this machine; AI on the server is fine.
    assert!(app.ai_on_server());

    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    assert!(!app.ai_on_server(), "no opt-in: AI must stay on the client");
    assert_eq!(app.ai_endpoint(), clew_protocol::AiEndpoint::Client);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.send_ai_config();
    // Without the opt-in the server is told to hold NOTHING — the message is
    // sent, carrying no keys. Sending nothing at all was the bug: it left a
    // server that had been granted the keys earlier still holding them.
    match rx.try_recv().expect("SetAiConfig is always sent").request {
        clew_protocol::Request::SetAiConfig { chat, embed } => {
            assert!(chat.is_none() && embed.is_none(), "no keys without opt-in");
        }
        other => panic!("expected SetAiConfig, got {other:?}"),
    }

    // With the opt-in (the Connect form checkbox), the endpoint flips.
    app.remote_ai_opt_in = true;
    assert!(app.ai_on_server());
    assert_eq!(app.ai_endpoint(), clew_protocol::AiEndpoint::Server);

    // Revoking it is an ACTIVE step: the server is already holding the keys,
    // so it must be told to drop them, not merely stop being sent new ones.
    while rx.try_recv().is_ok() {}
    app.set_remote_ai_opt_in(false);
    match rx
        .try_recv()
        .expect("revoking must reach the server")
        .request
    {
        clew_protocol::Request::SetAiConfig { chat, embed } => {
            assert!(
                chat.is_none() && embed.is_none(),
                "revocation clears the keys"
            );
        }
        other => panic!("expected SetAiConfig, got {other:?}"),
    }

    // And any transport switch drops the grant.
    app.remote_ai_opt_in = true;
    let _ = app.connect_to(crate::backend::connect::ConnTarget::Local);
    assert!(!app.remote_ai_opt_in);
}

/// Same-name methods in one file (different impls' `new`) get distinct explain
/// identities — they used to merge into one cache entry, and detail always
/// showed the first one's body.
#[test]
fn same_name_methods_get_distinct_explain_nodes() {
    let dir = std::env::temp_dir().join("clew-explain-ordinal-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pair.rs");
    std::fs::write(
        &file,
        "struct A;\nimpl A {\n    fn new() -> A {\n        A\n    }\n}\n\
         struct B;\nimpl B {\n    fn new() -> B {\n        B\n    }\n}\n",
    )
    .unwrap();

    let inputs = gather_explain_inputs(vec![file.clone()], dir.clone());
    let news: Vec<&explain::FnInput> = inputs
        .functions
        .iter()
        .filter(|f| f.name == "new")
        .collect();
    assert_eq!(news.len(), 2, "both `new`s gathered");
    let mut ordinals: Vec<u32> = news.iter().map(|f| f.ordinal).collect();
    ordinals.sort();
    assert_eq!(ordinals, vec![0, 1], "distinct identities");
    // Their bodies differ — each explains its own impl.
    assert_ne!(news[0].body, news[1].body);

    // The schedule keeps them as two nodes (no dedup-by-name).
    let groups = explain::schedule(&inputs);
    let fn_nodes: usize = groups
        .iter()
        .flat_map(|g| &g.nodes)
        .filter(|n| matches!(n, explain::Node::Function { name, .. } if name == "new"))
        .count();
    assert_eq!(fn_nodes, 2);

    // Detail gathering picks the right body for each ordinal.
    let empty = HashMap::new();
    let (_, body0, _) = gather_fn_detail_input(&dir, file.clone(), "new", 0, &empty).unwrap();
    let (_, body1, _) = gather_fn_detail_input(&dir, file.clone(), "new", 1, &empty).unwrap();
    assert!(body0.contains("A"), "{body0:?}");
    assert!(body1.contains("B"), "{body1:?}");
    assert_ne!(body0, body1);
}

/// The remote flow's `LspResolved` reply raises the approval modal for the
/// command line the SERVER resolved (the file lives there), bound to the
/// current project — and a stale reply (slot no longer waiting) is dropped.
#[test]
fn lsp_resolved_reply_raises_the_modal_for_the_remote_command() {
    let mut app = scanned_app("lsp-resolved");
    let root = app.project.as_ref().unwrap().root.clone();
    app.lsp.insert("rust".into(), LspSlot::AwaitingConsent);

    let reply = |app: &mut App, for_root: String, spec: clew_protocol::LspCommandSpec| {
        let _ = app.update(Message::ServerEvent {
            conn: app.conn_gen,
            msg: clew_protocol::ServerMessage::Reply {
                id: 99,
                sub: None,
                event: clew_protocol::Event::LspResolved {
                    language: "rust".into(),
                    root: for_root,
                    resolution: clew_protocol::LspResolution::Command(spec),
                },
            },
        });
    };

    reply(
        &mut app,
        root.to_string_lossy().into_owned(),
        clew_protocol::LspCommandSpec {
            command: "/remote/proj/run-lsp.sh".into(),
            args: vec!["--stdio".into()],
            server: "rust-analyzer".into(),
            version: "x".into(),
            fingerprint: "fp-remote".into(),
            init_options: None,
        },
    );
    let pending = app.pending_lsp_command.as_ref().expect("modal raised");
    assert_eq!(pending.root, root, "bound to the current project");
    assert!(
        pending
            .command_line()
            .is_some_and(|l| l.contains("/remote/proj/run-lsp.sh"))
    );
    assert_eq!(pending.fingerprint, "fp-remote");

    // A resolution computed for a DIFFERENT project (late reply straddling
    // an A→B switch) must not raise this project's approval modal.
    app.pending_lsp_command = None;
    app.lsp.insert("rust".into(), LspSlot::AwaitingConsent);
    reply(
        &mut app,
        "/somewhere/else".into(),
        clew_protocol::LspCommandSpec {
            command: "/old-project/run-lsp.sh".into(),
            args: vec![],
            server: "s".into(),
            version: "1".into(),
            fingerprint: "fp-old".into(),
            init_options: None,
        },
    );
    assert!(
        app.pending_lsp_command.is_none(),
        "another project's resolve reply must be dropped"
    );

    // A stale LspResolved (slot no longer waiting) must not raise anything.
    app.pending_lsp_command = None;
    app.lsp.insert("rust".into(), LspSlot::Starting);
    reply(
        &mut app,
        root.to_string_lossy().into_owned(),
        clew_protocol::LspCommandSpec {
            command: "/evil".into(),
            args: vec![],
            server: "s".into(),
            version: "1".into(),
            fingerprint: "fp-evil".into(),
            init_options: None,
        },
    );
    assert!(
        app.pending_lsp_command.is_none(),
        "a stale resolve reply must be dropped"
    );
}

/// Stopping during `Launching` — before the adapter handed back a client —
/// must cancel the startup, not just hide its events: the run identity moves
/// on, and the in-flight stream sees that at its next checkpoint.
#[test]
fn debug_stop_while_launching_cancels_the_startup() {
    let mut app = scanned_app("debug-cancel");
    app.debug.session = Some(DebugSession {
        client: None, // still Launching: nothing to disconnect
        status: DebugStatus::Launching,
        thread_id: None,
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: Vec::new(),
        current: None,
        program: PathBuf::from("/bin/true"),
        args: Vec::new(),
        cwd: PathBuf::from("/"),
        port: None,
    });
    app.bump_debug_run();
    let launching = app.debug_run;

    let _ = app.update(Message::DebugStop);
    assert!(app.debug.session.is_none());
    assert_ne!(app.debug_run, launching, "the run identity moved on");
    assert_eq!(
        app.debug_run_live.load(std::sync::atomic::Ordering::SeqCst),
        app.debug_run,
        "the live counter the startup stream polls must follow"
    );

    // The cancelled run's late DapStarted is ignored — no session resurrects.
    let _ = app.update(Message::DapEvent {
        run: launching,
        event: dap::DapEvent::Terminated,
    });
    assert!(app.debug.session.is_none());
}

/// A→B→A: re-selecting the file already shown cancels B's in-flight load, so
/// B's late reply can't replace the A the user is looking at.
#[test]
fn reopening_the_current_file_cancels_the_pending_load() {
    let mut app = scanned_app("open-aba");
    let root = app.project.as_ref().unwrap().root.clone();
    open_synchronously(&mut app, "src/lib.rs", None); // A is shown

    // Start opening B (its load is in flight).
    let _ = app.update(Message::OpenRel {
        rel: "notes.txt".into(),
        line: None,
    });
    let req_b = app.pane_pending[0].expect("B is loading");

    // The user goes back to A, which is still in the pane: the same-file
    // fast path must cancel B rather than leave it pending.
    let _ = app.update(Message::OpenRel {
        rel: "src/lib.rs".into(),
        line: Some(3),
    });
    assert_eq!(app.active_viewer().unwrap().rel, "src/lib.rs");
    assert!(app.pane_pending[0].is_none(), "B's load must be cancelled");

    // B's reply arrives late and is dropped.
    let _ = app.update(Message::FileLoaded {
        req: req_b,
        pane: 0,
        abs: root.join("notes.txt"),
        target: None,
        result: Ok("needle in notes\n".into()),
    });
    assert_eq!(
        app.active_viewer().unwrap().rel,
        "src/lib.rs",
        "a cancelled load must not replace the current file"
    );
}

/// A streamed token must reach the turn it belongs to and no other. Routing
/// by "the last streaming turn" meant a delta from a superseded stream — a
/// newer question, Ask Clear, or a project switch — was appended to whatever
/// conversation happened to be open.
#[test]
fn a_delta_from_a_superseded_stream_never_lands_in_another_turn() {
    let mut app = scanned_app("ask-stream-routing");
    let turn = |stream: u64, q: &str| AskTurn {
        stream,
        question: q.into(),
        answer_md: String::new(),
        answer: Vec::new(),
        sources: Vec::new(),
        steps: Vec::new(),
        streaming: true,
    };
    app.ask_turns.push(turn(7, "the old question"));
    app.ask_turns.push(turn(9, "the current question"));

    let _ = app.update(Message::AskDelta {
        stream: 7,
        text: "old answer".into(),
    });
    assert_eq!(app.ask_turns[0].answer_md, "old answer");
    assert_eq!(
        app.ask_turns[1].answer_md, "",
        "a delta must not land in a turn it does not belong to"
    );

    // A stream nobody is listening to is dropped rather than appended anywhere.
    let _ = app.update(Message::AskDelta {
        stream: 999,
        text: "orphan".into(),
    });
    assert_eq!(app.ask_turns[0].answer_md, "old answer");
    assert_eq!(app.ask_turns[1].answer_md, "");

    // The same for steps, and for the turn-ending message: a late Done from a
    // superseded turn must not release the Stop button of a live one.
    app.agent_stream = Some(9);
    let _ = app.update(Message::AgentTurnEnded {
        stream: 7,
        error: None,
    });
    assert_eq!(
        app.agent_stream,
        Some(9),
        "a superseded turn must not unblock the gate for the live one"
    );
    assert!(app.ask_turns[1].streaming, "the live turn is still open");
}

/// Leaving a project must stop the work running FOR it, not merely ignore
/// that work's results: an agent turn keeps calling tools (and spending) on
/// the server, and a debuggee keeps running and feeding the Ask context.
#[test]
fn opening_another_project_stops_the_previous_projects_work() {
    let mut app = scanned_app("teardown-a");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.agent_stream = Some(42);
    app.indexing = true;
    app.stats.building = true;
    app.overview.generating = true;
    app.building_embeddings = true;

    let root_b = fixture_project("teardown-b");
    scan_synchronously(&mut app, root_b);

    assert_eq!(
        app.agent_stream, None,
        "the old turn's id must be released, or the new project's Ask stays gated"
    );
    let stop = rx.try_recv().expect("an AgentStop must be sent");
    assert!(
        matches!(
            stop.request,
            clew_protocol::Request::AgentStop { stream: 42 }
        ),
        "got {:?}",
        stop.request
    );
    // The new project re-arms its own indexing, so assert the teardown's flag
    // clearing directly — a transport switch has no new scan to do it.
    let mut busy = scanned_app("teardown-c");
    busy.indexing = true;
    busy.stats.building = true;
    busy.overview.generating = true;
    busy.building_embeddings = true;
    busy.docs.loading = true;
    busy.project_calls.building = true;
    let _ = busy.drop_project_work();
    assert!(!busy.indexing && !busy.stats.building);
    assert!(!busy.overview.generating && !busy.building_embeddings);
    assert!(!busy.docs.loading && !busy.project_calls.building);
}

/// A blame reply must paint the file its request named. It used to re-derive
/// the path from the CURRENT root plus the reply's rel, so after switching to
/// a project that has a file at the same relative path, the old project's
/// blame painted the new project's file.
#[test]
fn a_blame_reply_cannot_paint_a_different_projects_file() {
    let mut app = scanned_app("blame-a");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    let abs_a = app.project.as_ref().unwrap().root.join("src/lib.rs");

    // A blame request went out for project A's lib.rs…
    app.pending_git.insert(77, abs_a.clone());

    // …then the user switched to project B, which has the same rel path.
    let root_b = fixture_project("blame-b");
    scan_synchronously(&mut app, root_b.clone());
    assert!(
        app.pending_git.is_empty(),
        "leaving a project drops its in-flight blame"
    );

    // The late reply names an id nobody is waiting for, so it paints nothing.
    let _ = app.update(Message::ServerEvent {
        conn: app.conn_gen,
        msg: clew_protocol::ServerMessage::Reply {
            id: 77,
            sub: None,
            event: clew_protocol::Event::GitInfo {
                rel: "src/lib.rs".into(),
                info: Some(clew_protocol::GitInfo::default()),
            },
        },
    });
    assert!(
        app.panes.iter().flatten().all(|v| v.git.is_none()),
        "a blame reply for a project we left must not paint this one"
    );
}

/// A hover result must belong to the peek that is actually open. Matching on
/// (line, col) alone let a result land after the pane's document had been
/// replaced under a motionless cursor — same coordinates, different file.
#[test]
fn a_hover_result_from_a_superseded_peek_is_dropped() {
    let mut app = scanned_app("hover-gen");
    let peek = || HoverState {
        line: 3,
        col: 5,
        x: 0.0,
        y: 0.0,
        text: None,
        summary: None,
        diagnostic: None,
    };
    app.hover = Some(peek());
    let epoch = app.hover_gen;

    // The document changes: the peek is invalidated and the generation moves.
    app.invalidate_hover();
    app.hover = Some(peek());

    let _ = app.update(Message::HoverResult {
        epoch,
        line: 3,
        col: 5,
        text: Some("stale type".into()),
    });
    assert!(
        app.hover.as_ref().unwrap().text.is_none(),
        "a result for the previous peek must not paint the current one"
    );

    // The current generation still paints.
    let _ = app.update(Message::HoverResult {
        epoch: app.hover_gen,
        line: 3,
        col: 5,
        text: Some("fresh type".into()),
    });
    assert_eq!(
        app.hover.as_ref().unwrap().text.as_deref(),
        Some("fresh type")
    );
}

/// A markdown link is untrusted text — an LLM answer or the repository's own
/// prose. It used to be joined onto the project root and probed on disk, so
/// `../../` reached outside the project entirely.
#[test]
fn a_markdown_link_cannot_escape_the_project() {
    let app = scanned_app("link-escape");
    for escape in [
        "../../../../etc/passwd",
        "../outside.rs",
        "/etc/passwd",
        "src/../../escape.rs",
    ] {
        assert!(
            app.resolve_project_link(escape).is_none(),
            "{escape} must not resolve"
        );
    }
    // A real project file still resolves, with and without a `./` prefix and
    // with a line fragment.
    assert!(app.resolve_project_link("src/lib.rs").is_some());
    assert!(app.resolve_project_link("./src/lib.rs").is_some());
    let (_, line) = app.resolve_project_link("src/lib.rs#L3").expect("resolves");
    assert_eq!(line, Some(3));
}

/// The sibling of the test above, for the `clew:` scheme. That scheme is only
/// ever MINTED by `linkify_citations`, but nothing forces a `clew:` URL to have
/// come from there: iced's markdown widget hands any link destination through
/// verbatim, and the markdown clew renders is untrusted — a repository README,
/// a `///` doc comment in the Docs tab, a prompt-injected LLM answer. The
/// branch used to join the target onto the root unchecked, and `Path::join`
/// DISCARDS the root when the argument is absolute, so `[Setup](clew:/etc/passwd)`
/// opened that file in the editor. The `..` form was already refused downstream
/// (it stays lexically under the root and so goes through the server's
/// `confine`); the absolute form escaped, and so did a symlinked directory
/// component, which the local read fallback does not check either.
#[test]
fn a_clew_citation_link_cannot_escape_the_project() {
    let mut app = scanned_app("clew-link-escape");
    let root = app.project.as_ref().unwrap().root.clone();
    let pane = app.active;

    // A real project rel still opens: the pane gets a load token.
    let _ = app.update(Message::OpenLink("clew:src/lib.rs:3".into()));
    assert!(
        app.pane_pending[pane].is_some(),
        "an in-project citation must still open, status: {}",
        app.status
    );
    app.pane_pending[pane] = None;

    for escape in [
        "clew:/etc/passwd",
        "clew:/etc/passwd:3",
        "clew:../../../../etc/passwd",
        "clew:",
    ] {
        let _ = app.update(Message::OpenLink(escape.into()));
        assert!(
            app.pane_pending[pane].is_none(),
            "{escape} must not start a load"
        );
        assert!(
            app.status.starts_with("Refused a link outside the project"),
            "{escape} must be visibly refused, got: {}",
            app.status
        );
    }

    // A symlinked directory component inside the project passes every lexical
    // check, so containment is re-decided against the real filesystem.
    #[cfg(unix)]
    {
        let outside = root.parent().unwrap().join("clew-link-escape-outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let link = root.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let _ = app.update(Message::OpenLink("clew:link/secret.txt".into()));
        assert!(
            app.pane_pending[pane].is_none(),
            "a symlinked component must not start a load"
        );
        assert!(
            app.status.contains("not a file inside the project"),
            "got: {}",
            app.status
        );
    }
    let _ = root;
}

/// `OpenRel` carries a rel from a list the UI rendered. Every list that feeds
/// it today is safe by construction, but the sidebar tree and the Docs index of
/// a REMOTE project are built from what the server sent, so the rel is only as
/// trustworthy as that server. `join` discards the root for an absolute
/// argument, so the check has to happen before it, and it has to be lexical so
/// the remote case never probes this machine's disk.
#[test]
fn open_rel_refuses_a_path_that_escapes_the_project() {
    let mut app = scanned_app("open-rel-escape");
    let pane = app.active;

    // The ordinary case a sidebar click produces still opens.
    let _ = app.update(Message::OpenRel {
        rel: "src/lib.rs".into(),
        line: Some(3),
    });
    assert!(
        app.pane_pending[pane].is_some(),
        "an in-project rel must still open, status: {}",
        app.status
    );
    app.pane_pending[pane] = None;

    for escape in ["/etc/passwd", "../../../../etc/passwd", ""] {
        let _ = app.update(Message::OpenRel {
            rel: escape.into(),
            line: None,
        });
        assert!(
            app.pane_pending[pane].is_none(),
            "{escape:?} must not start a load"
        );
        assert!(
            app.status.starts_with("Refused a path outside the project"),
            "{escape:?} must be visibly refused, got: {}",
            app.status
        );
    }
}

/// The same guard on a REMOTE project must be decided WITHOUT probing this
/// machine's disk: the remote's paths name another host's files, so a local
/// `is_inside` check would both leak onto this filesystem and refuse every
/// legitimate citation whose path happens not to exist here. The lexical half
/// of the check still applies, and the server's own `confine` is the second gate.
#[test]
fn a_clew_citation_link_on_a_remote_project_is_not_resolved_locally() {
    let mut app = blank_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    // A root that exists only on the remote host.
    let root = PathBuf::from("/remote-only/proj");
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    let _ = app.handle_server_reply(
        1,
        clew_protocol::Event::Tree {
            root: root.to_string_lossy().into_owned(),
            tree: clew_protocol::DirNode {
                dirs: Vec::new(),
                files: Vec::new(),
            },
            files: vec!["src/lib.rs".into()],
            truncated: false,
        },
    );
    assert!(app.project.is_some(), "the remote project must open");
    while rx.try_recv().is_ok() {} // drain the open's own traffic

    let _ = app.update(Message::OpenLink("clew:src/lib.rs:3".into()));
    let mut asked = false;
    while let Ok(msg) = rx.try_recv() {
        if let clew_protocol::Request::ReadFile { rel, .. } = &msg.request
            && rel == "src/lib.rs"
        {
            asked = true;
        }
    }
    assert!(
        asked,
        "a remote citation must be dispatched to the server, not decided against local disk"
    );

    // The lexical refusal still applies remotely, and sends nothing.
    let _ = app.update(Message::OpenLink("clew:/etc/passwd".into()));
    assert!(
        app.status.starts_with("Refused a link outside the project"),
        "got: {}",
        app.status
    );
    while let Ok(msg) = rx.try_recv() {
        assert!(
            !matches!(msg.request, clew_protocol::Request::ReadFile { .. }),
            "a refused link must not reach the server"
        );
    }
}

/// A repository must not be able to hand clew a symbol index. Entries used to
/// be reused on a content hash the repository can compute for its own files,
/// from a cache inside the project — so committing `.clew/cache/index.json`
/// forged clew's own conclusions: navigation, both graphs, and the source it
/// hands to the model, with no code execution at all.
#[test]
fn a_planted_index_cache_in_the_project_is_ignored() {
    let root = fixture_project("forged-cache");
    let data = root.parent().unwrap().join("forged-cache-data");
    let _ = std::fs::remove_dir_all(&data);
    let _env = data_dir_override(&data);
    // A cache the "repository" ships, with a correct hash for the real file
    // and a symbol that does not exist in it.
    let src = std::fs::read(root.join("src/lib.rs")).unwrap();
    let hash = incremental::content_hash(&src);
    let meta = std::fs::metadata(root.join("src/lib.rs")).unwrap();
    let planted = format!(
        r#"{{"version":{},"entries":{{"src/lib.rs":{{"mtime_ns":{},"size":{},"hash":{},
           "symbols":[{{"name":"forged_symbol","kind":"function","line":1,"is_test":false}}],
           "imports":[]}}}}}}"#,
        crate::session::cache::CACHE_VERSION,
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        meta.len(),
        hash,
    );
    std::fs::create_dir_all(root.join(".clew/cache")).unwrap();
    std::fs::write(root.join(".clew/cache/index.json"), planted).unwrap();

    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    let store = app.derived_dir.clone().expect("a derived store");
    let files = app.project.as_ref().unwrap().files.clone();
    let indexed = index::build_indexed_warm(&root, Some(&store), files);

    assert!(
        !indexed
            .by_file
            .values()
            .flatten()
            .any(|s| s.name == "forged_symbol"),
        "a cache shipped inside the project must never be read"
    );
    assert!(
        indexed
            .by_file
            .values()
            .flatten()
            .any(|s| s.name == "origin"),
        "the real file is indexed"
    );
    // And clew's own store is where the index actually landed.
    assert!(store.join("index.json").exists());
    assert!(
        !store.starts_with(&root),
        "the store is outside the project"
    );
}

/// A Stats run that FAILED (a dead transport, the RPC timeout, a refusal on a
/// remote project) must not be committed as a completed, empty report. It was:
/// the view then claimed the project has no code files, the status bar said
/// "Code statistics ready", the empty report went into the derived cache, and
/// the revision stamp made it look fresh, so re-entering the view never
/// retried.
#[test]
fn a_failed_stats_run_is_not_cached_as_a_fresh_empty_report() {
    let root = fixture_project("stats-failed-run");
    let data = root.parent().unwrap().join("stats-failed-run-data");
    let _ = std::fs::remove_dir_all(&data);
    let _env = data_dir_override(&data);
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    let store = app.derived_dir.clone().expect("a derived store");

    // A good report from an earlier run is on screen.
    let good = stats::StatsReport {
        langs: vec![stats::LangStat {
            name: "Rust".into(),
            files: 1,
            code: 42,
            comments: 0,
            blanks: 0,
        }],
        ..Default::default()
    };
    app.stats.report = Some(good.clone());
    app.stats.building = true;

    // The failed run, exactly as `start_stats` reports one.
    let failed = crate::app::server_ai::stats_done(root.clone(), app.project_epoch, 7, None);
    let _ = app.update(failed);

    assert!(!app.stats.building, "the spinner must stop");
    assert_eq!(
        app.stats.report.as_ref().map(|r| r.langs.len()),
        Some(1),
        "the last good report must survive a failed run"
    );
    assert_ne!(
        app.stats.rev,
        app.registry.revision(),
        "a failed run must not count as fresh, or the view never retries"
    );
    assert!(
        !app.status.contains("ready"),
        "a failure must not be announced as success, got {:?}",
        app.status
    );
    assert!(
        stats::load(&store).is_none(),
        "nothing may be written to the derived cache for a failed run"
    );

    // A run that really did complete still commits, cache included.
    let done = crate::app::server_ai::stats_done(root, app.project_epoch, 7, Some(good));
    let _ = app.update(done);
    assert_eq!(app.stats.rev, 7);
    assert_eq!(stats::load(&store).map(|c| c.rev), Some(7));
}

/// An Explain pass the LLM ABORTED (expired key, no quota) reports the cache it
/// managed to build, which starts empty and stays empty when every call is
/// rejected. Folding that in as a completed pass — keeping only the nodes it
/// holds — deletes a whole project's stored explanations, i.e. thousands of
/// billed calls, because a key expired. A completed pass must still prune, or
/// summaries of deleted files linger forever.
#[test]
fn an_aborted_explain_pass_does_not_prune_the_stored_summaries() {
    let root = fixture_project("explain-aborted-pass");
    let data = root.parent().unwrap().join("explain-aborted-pass-data");
    let _ = std::fs::remove_dir_all(&data);
    let _env = data_dir_override(&data);
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    let store = app.derived_dir.clone().expect("a derived store");

    // An earlier pass explained two nodes; both windows' copies hold them.
    let node = |name: &str| explain::Node::Function {
        file: root.join("src/lib.rs"),
        name: name.into(),
        ordinal: 0,
    };
    let cached = |s: &str| explain::Cached {
        summary: s.into(),
        prompt_hash: 1,
        detail: None,
    };
    let mut stored = explain::Cache::new();
    stored.insert(node("origin"), cached("what origin does"));
    stored.insert(node("helper"), cached("what helper does"));
    explain::save(&store, &stored).unwrap();
    app.explain.cache = stored.clone();
    app.explain.running = true;

    // The rejected key: the pass broke out of its first level, so the cache it
    // carries is empty.
    let _ = app.update(Message::ExplainDone {
        root: root.clone(),
        generation: app.explain.generation,
        cache: explain::Cache::new(),
        failed: 3,
        auth_error: Some("401 invalid api key".into()),
    });
    assert_eq!(
        explain::load(&store, &root).len(),
        2,
        "a rejected key must not delete explanations that were already paid for"
    );
    assert_eq!(
        app.explain.cache.len(),
        2,
        "and the window keeps showing them"
    );
    assert!(app.status.contains("rejected"), "got {:?}", app.status);

    // A pass that really completed still prunes: `helper`'s file is gone from
    // the project, and its summary must not linger.
    let mut finished = explain::Cache::new();
    finished.insert(node("origin"), cached("what origin does now"));
    let _ = app.update(Message::ExplainDone {
        root: root.clone(),
        generation: app.explain.generation,
        cache: finished,
        failed: 0,
        auth_error: None,
    });
    let on_disk = explain::load(&store, &root);
    assert_eq!(on_disk.len(), 1, "a completed pass prunes what it dropped");
    assert_eq!(
        on_disk.get(&node("origin")).map(|c| c.summary.as_str()),
        Some("what origin does now"),
    );
}

/// "Mark understood" must land the value the reader chose, not the opposite of
/// whatever the file happens to hold. The change is replayed inside
/// `notes::edit` on the list read from disk under the lock — by construction
/// the state this window has NOT seen, since `self.notes` is its snapshot from
/// project open. Replaying a flip there inverted the click: a second window
/// that had already marked the symbol had its flag cleared and its entry
/// deleted, while the window that clicked showed no change at all.
#[test]
fn marking_understood_lands_the_readers_value_over_a_changed_file() {
    let mut app = scanned_app("note-understood-stale");
    let root = app.project.as_ref().unwrap().root.clone();
    assert!(app.notes.is_empty(), "this window's open-time snapshot");

    // A second window on the same project marks the symbol understood; this
    // window still renders it unchecked and cannot know.
    notes::edit(&root, |list| {
        notes::set_understood(list, "src/lib.rs", "origin", true)
    })
    .1
    .unwrap();

    // The reader clicks the checkbox they see: their intent is "understood".
    let _ = app.update(Message::NoteToggleUnderstood {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
    });
    assert!(
        notes::find(&notes::load(&root), "src/lib.rs", "origin").is_some_and(|n| n.understood),
        "the click must not clear the flag the other window set"
    );
    assert!(
        notes::find(&app.notes, "src/lib.rs", "origin").is_some_and(|n| n.understood),
        "and this window adopts the merged file, so the box reads checked"
    );

    // Unmarking still unmarks: the value now resolves to false and the note,
    // carrying nothing else, is dropped.
    let _ = app.update(Message::NoteToggleUnderstood {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
    });
    assert!(notes::load(&root).is_empty());
    assert!(app.notes.is_empty());
}

/// A project whose `.clew/` cannot be written must not swallow the note the
/// user just typed. `NoteEditSave` takes the draft and closes the editor
/// BEFORE saving, so a store that returned only the error left the prose
/// nowhere at all — worse than the whole-file write it replaced, which at
/// least kept the note on screen for the session.
#[test]
fn a_failed_note_save_keeps_the_text_in_the_session() {
    let mut app = scanned_app("note-save-unwritable");
    let root = app.project.as_ref().unwrap().root.clone();
    // `.clew` as a plain FILE: `write_atomic` refuses every state write under
    // it (the same refusal a read-only checkout or a full disk produces).
    let _ = std::fs::remove_dir_all(root.join(".clew"));
    std::fs::write(root.join(".clew"), "not a dir").unwrap();

    let _ = app.update(Message::NoteEditStart {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
    });
    let _ = app.update(Message::NoteEditInput("three paragraphs of prose".into()));
    let _ = app.update(Message::NoteEditSave);

    assert!(
        app.reading_note_edit.is_none(),
        "the editor closes on save, which is why the text must live in `notes`"
    );
    assert_eq!(
        notes::find(&app.notes, "src/lib.rs", "origin").map(|n| n.text.as_str()),
        Some("three paragraphs of prose"),
        "the unsaved note must stay usable for the session"
    );
    assert!(
        app.status.contains("not saved"),
        "the failure must be named, got {:?}",
        app.status
    );
    assert!(
        notes::load(&root).is_empty(),
        "nothing reached disk — the store was unwritable"
    );
}

/// The same for a bookmark note, on the shape that actually occurs on a
/// read-only checkout: the store READS fine and only the write fails, so the
/// merged list carries the typed text and the window must adopt it.
#[cfg(unix)]
#[test]
fn a_failed_bookmark_note_save_keeps_the_text_in_the_session() {
    use std::os::unix::fs::PermissionsExt;

    let mut app = scanned_app("bookmark-note-unwritable");
    let root = app.project.as_ref().unwrap().root.clone();
    let seed = vec![bookmarks::Bookmark {
        rel: "src/lib.rs".into(),
        line: 3,
        preview: "pub fn origin() -> Point {".into(),
        note: None,
    }];
    bookmarks::save(&root, &seed).unwrap();
    app.bookmarks = bookmarks::load(&root);
    // Read-only `.clew/`: the load below still works, the temp file the atomic
    // write needs cannot be created.
    let clew = root.join(".clew");
    let writable = std::fs::metadata(&clew).unwrap().permissions();
    std::fs::set_permissions(&clew, std::fs::Permissions::from_mode(0o500)).unwrap();

    let _ = app.update(Message::BookmarkNoteEdit("src/lib.rs".into(), 3));
    let _ = app.update(Message::BookmarkNoteInput("why this line matters".into()));
    let _ = app.update(Message::BookmarkNoteSave);

    // Restored before the assertions: a panic with the directory still
    // read-only would leave a fixture no later run could clean up.
    std::fs::set_permissions(&clew, writable).unwrap();

    assert!(app.note_edit.is_none(), "the editor closed on save");
    assert_eq!(
        app.bookmarks.first().and_then(|b| b.note.as_deref()),
        Some("why this line matters"),
        "the unsaved note must stay usable for the session"
    );
    assert!(
        app.status.contains("not saved"),
        "the failure must be named, got {:?}",
        app.status
    );
    assert_eq!(
        bookmarks::load(&root).first().and_then(|b| b.note.clone()),
        None,
        "nothing reached disk — the store was read-only"
    );
}

/// Two windows on one project: this window's note edit must not erase the note
/// the other one wrote after this window loaded its copy. `save_notes` wrote
/// `self.notes` back wholesale, so the other window's work vanished with no
/// sign until the next launch — each window kept rendering its own copy.
#[test]
fn a_note_edit_keeps_the_other_windows_note() {
    let mut app = scanned_app("notes-two-windows");
    let root = app.project.as_ref().unwrap().root.clone();
    // This window's snapshot from project open.
    assert!(app.notes.is_empty());

    // The other window annotates a symbol and persists it.
    let mut theirs = Vec::new();
    notes::set_text(&mut theirs, "src/lib.rs", "Point", "theirs");
    notes::save(&root, &theirs).unwrap();

    // This window annotates a different symbol, from its stale snapshot.
    let _ = app.update(Message::NoteToggleUnderstood {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
    });

    let on_disk = notes::load(&root);
    assert!(
        on_disk.iter().any(|n| n.symbol == "Point"),
        "the other window's note must survive this window's edit, got {:?}",
        on_disk
            .iter()
            .map(|n| n.symbol.as_str())
            .collect::<Vec<_>>()
    );
    assert!(on_disk.iter().any(|n| n.symbol == "origin"));
    assert_eq!(app.notes, on_disk, "the window adopts what was written");

    // And a removal still removes, rather than being lost in a merge.
    let _ = app.update(Message::NoteRemove {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
    });
    let symbols: Vec<_> = notes::load(&root)
        .iter()
        .map(|n| n.symbol.clone())
        .collect();
    assert_eq!(symbols, ["Point"]);
}

/// Two windows on one project: generating a tour must not wipe the tour the
/// other window generated after this one loaded its library. The whole-library
/// write did exactly that, and the losing window kept showing its own copy.
#[test]
fn a_generated_walkthrough_keeps_the_other_windows_tour() {
    let mut app = scanned_app("walk-two-windows");
    let root = app.project.as_ref().unwrap().root.clone();
    // This window's library from project open.
    assert!(app.walk.library.is_empty());

    // The other window generates and persists a tour.
    std::fs::create_dir_all(root.join(".clew").join("cache")).unwrap();
    let theirs = walkthrough::Walkthrough {
        title: "Theirs".into(),
        scope: "theirs".into(),
        steps: vec![walkthrough::Step {
            title: "s".into(),
            file: "src/lib.rs".into(),
            symbol: None,
            line: Some(1),
            narration: "n".into(),
        }],
    };
    walkthrough::save_library(&root, &[theirs]).unwrap();

    // This window's generation lands, from its stale (empty) library.
    let mine = walkthrough::Walkthrough {
        title: "Mine".into(),
        scope: String::new(),
        steps: vec![walkthrough::Step {
            title: "s".into(),
            file: "src/lib.rs".into(),
            symbol: None,
            line: Some(1),
            narration: "n".into(),
        }],
    };
    let _ = app.update(Message::WalkthroughDone {
        root: root.clone(),
        epoch: app.project_epoch,
        scope: "mine".into(),
        result: Ok(mine),
    });

    let scopes: Vec<String> = walkthrough::load_library(&root)
        .iter()
        .map(|w| w.scope.clone())
        .collect();
    assert!(
        scopes.contains(&"theirs".to_string()),
        "the other window's tour must survive this one's generation, got {scopes:?}"
    );
    assert!(scopes.contains(&"mine".to_string()));
    // The open tour is resolved by scope, so the merge's reordering cannot
    // leave it pointing at somebody else's tour.
    let open = app.walk.open.expect("the generated tour is opened");
    assert_eq!(app.walk.library[open].scope, "mine");
}

/// A tour's JSON as the remote `.clew/cache/walkthroughs.json` stores it.
fn tour_json(scope: &str) -> String {
    format!(
        r#"{{"title":"{scope}","scope":"{scope}","steps":[{{"title":"s","file":"src/lib.rs","line":1,"narration":"n"}}]}}"#
    )
}

/// A remote App with a project open, wired to a channel the test reads the
/// outgoing requests from.
fn remote_app(
    tag: &str,
) -> (
    App,
    PathBuf,
    tokio::sync::mpsc::UnboundedReceiver<clew_protocol::ClientMessage>,
) {
    let root = fixture_project(tag);
    let mut app = blank_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    let _ = app.handle_server_reply(
        1,
        clew_protocol::Event::Tree {
            root: root.to_string_lossy().into_owned(),
            tree: clew_protocol::DirNode {
                dirs: Vec::new(),
                files: Vec::new(),
            },
            files: vec!["src/lib.rs".into()],
            truncated: false,
        },
    );
    (app, root, rx)
}

/// The remote twin of `a_generated_walkthrough_keeps_the_other_windows_tour`.
///
/// `walk.open` is a positional INDEX into `walk.library`, and what a remote
/// merge replies with is the FILE's list in the FILE's order — a tour another
/// client appended ahead of this window's, or removed, moves every index this
/// window computed. Adopting the merged library without rebasing that index
/// left the WALK pane highlighting and narrating somebody else's tour, and
/// next/prev navigating the editor into that tour's files. Both LOCAL
/// mutation paths already re-resolve by scope; both REMOTE adoptions
/// (`StateEdited` after a merge, `StateContent` on reconnect) must too.
#[test]
fn an_adopted_remote_walkthrough_library_keeps_the_open_tour_by_scope() {
    let (mut app, root, mut rx) = remote_app("remote-walk-merge");
    let root_s = root.to_string_lossy().into_owned();

    // This window loaded a library holding only its own tour, and opened it.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s.clone(),
        rel: walkthrough::LIBRARY_REL.into(),
        text: Some(format!("[{}]", tour_json("mine"))),
    });
    assert_eq!(app.walk.library.len(), 1);
    app.walk.open = Some(0);
    let (prepared, _) = app.prepare_segments("mine narration");
    assert!(!prepared.is_empty());
    app.walk.prepared = prepared;

    // It regenerates that tour, which travels as an upsert by scope.
    let mine = walkthrough::Walkthrough {
        title: "Mine".into(),
        scope: "mine".into(),
        steps: vec![walkthrough::Step {
            title: "s".into(),
            file: "src/lib.rs".into(),
            symbol: None,
            line: Some(1),
            narration: "n".into(),
        }],
    };
    app.save_walkthrough_scope("mine", Some(&mine));
    let mut edit_id = None;
    while let Ok(msg) = rx.try_recv() {
        if let clew_protocol::Request::EditState { rel, .. } = &msg.request
            && rel == walkthrough::LIBRARY_REL
        {
            edit_id = Some(msg.id);
        }
    }
    let edit_id = edit_id.expect("the tour change went out as a merge");

    // The server merges onto the file, which meanwhile gained another
    // client's tour AHEAD of this window's — so index 0 is no longer "mine".
    let _ = app.handle_server_reply(
        edit_id,
        clew_protocol::Event::StateEdited {
            root: root_s.clone(),
            rel: walkthrough::LIBRARY_REL.into(),
            text: Some(format!("[{},{}]", tour_json("theirs"), tour_json("mine"))),
        },
    );
    assert_eq!(app.walk.library.len(), 2, "the merged library is adopted");
    let open = app.walk.open.expect("the open tour survives the merge");
    assert_eq!(
        app.walk.library[open].scope, "mine",
        "the open tour is re-resolved by scope, not left on its old index"
    );

    // The reconnect re-read goes through the same adoption. Here the tour this
    // window had open is gone from the file, so nothing selects it — and its
    // narration must go with it rather than sitting under an empty selection.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s,
        rel: walkthrough::LIBRARY_REL.into(),
        text: Some(format!("[{}]", tour_json("theirs"))),
    });
    assert!(
        app.walk.open.is_none(),
        "a tour another client deleted must not leave the index on its neighbour"
    );
    assert!(app.walk.prepared.is_empty());
}

/// `edit_remote_state` deliberately supersedes an earlier in-flight edit of the
/// same file, because only the newest one owns that file's unsaved mark. The
/// reply to the superseded edit describes the file BEFORE the newer edit, so
/// adopting it rolled this window's copy back below its own optimistic change:
/// the entry the user had just acted on came back from the dead until the newer
/// reply landed, a re-press in that window sent a TOGGLE the server applied to
/// its newer truth (deleting outright what the user meant to keep), and if the
/// link died in the gap that rolled-back copy is what the reconnect flushed
/// wholesale over the server's file. Only the reply that still owns the rel may
/// be adopted.
#[test]
fn a_superseded_state_edit_reply_does_not_roll_this_window_back() {
    let (mut app, root, mut rx) = remote_app("remote-state-supersede");
    let root_s = root.to_string_lossy().into_owned();
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s.clone(),
        rel: "bookmarks.json".into(),
        text: Some(
            r#"[{"rel":"a.rs","line":1,"preview":"a"},{"rel":"b.rs","line":2,"preview":"b"},{"rel":"c.rs","line":3,"preview":"c"}]"#
                .into(),
        ),
    });
    assert_eq!(app.bookmarks.len(), 3);

    // Two removals inside one round trip: the second supersedes the first.
    let _ = app.on_bookmark_removed(0);
    let _ = app.on_bookmark_removed(0);
    let mut ids = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let clew_protocol::Request::EditState { rel, .. } = &msg.request
            && rel == "bookmarks.json"
        {
            ids.push(msg.id);
        }
    }
    assert_eq!(ids.len(), 2, "both removals went out as merges");
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(seen, ["c.rs"], "both removals are applied optimistically");

    // The FIRST edit's reply arrives: the file after removal one, before
    // removal two. It is older than what this window already holds.
    let _ = app.handle_server_reply(
        ids[0],
        clew_protocol::Event::StateEdited {
            root: root_s.clone(),
            rel: "bookmarks.json".into(),
            text: Some(
                r#"[{"rel":"b.rs","line":2,"preview":"b"},{"rel":"c.rs","line":3,"preview":"c"}]"#
                    .into(),
            ),
        },
    );
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(
        seen,
        ["c.rs"],
        "a superseded reply must not resurrect an entry this window already removed"
    );
    assert!(
        app.remote_state_dirty.contains("bookmarks.json"),
        "and it does not clear the mark the newer edit owns"
    );

    // The newest reply carries the server's cumulative merge — including a
    // bookmark another client added — and IS adopted.
    let _ = app.handle_server_reply(
        ids[1],
        clew_protocol::Event::StateEdited {
            root: root_s,
            rel: "bookmarks.json".into(),
            text: Some(
                r#"[{"rel":"c.rs","line":3,"preview":"c"},{"rel":"d.rs","line":4,"preview":"theirs"}]"#
                    .into(),
            ),
        },
    );
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(seen, ["c.rs", "d.rs"], "the window converges on the truth");
    assert!(!app.remote_state_dirty.contains("bookmarks.json"));
}

/// The bookmarks file every unsent-change test starts from.
const THREE_BOOKMARKS: &str = r#"[{"rel":"a.rs","line":1,"preview":"a"},{"rel":"b.rs","line":2,"preview":"b"},{"rel":"c.rs","line":3,"preview":"c"}]"#;

/// A remote window holding `THREE_BOOKMARKS`, one removal queued into a
/// transport that died before acknowledging it, and a fresh transport up with
/// the re-read outstanding. Returns the new outbox.
fn window_with_an_unsent_removal(
    tag: &str,
) -> (
    App,
    String,
    tokio::sync::mpsc::UnboundedReceiver<clew_protocol::ClientMessage>,
) {
    let (mut app, root, _rx) = remote_app(tag);
    let root_s = root.to_string_lossy().into_owned();
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s.clone(),
        rel: "bookmarks.json".into(),
        text: Some(THREE_BOOKMARKS.into()),
    });
    assert_eq!(app.bookmarks.len(), 3);

    // The link is already dead but nothing has noticed: this removal is queued
    // into a pipe that goes nowhere, and the status line reports it as saved.
    let _ = app.on_bookmark_removed(0);
    assert!(app.remote_state_dirty.contains("bookmarks.json"));

    // The death is detected, and a new transport comes up behind it.
    app.drop_connection_state();
    assert!(
        app.remote_state_unsent.contains("bookmarks.json"),
        "a change whose transport died unacknowledged is known to be unsent"
    );
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.request_remote_state();
    (app, root_s, rx)
}

/// Take every `EditState` id and every `WriteState` (id, payload) for `rel`
/// sitting in the outbox. The write ids matter: the acknowledgement of a
/// rescue flush is what retires the unsent mark, so a test that cannot name
/// that id cannot deliver its `StateWritten`.
fn took_state_writes(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<clew_protocol::ClientMessage>,
    want: &str,
) -> (Vec<u64>, Vec<(u64, Option<String>)>) {
    let (mut edits, mut writes) = (Vec::new(), Vec::new());
    while let Ok(msg) = rx.try_recv() {
        match &msg.request {
            clew_protocol::Request::EditState { rel, .. } if rel == want => edits.push(msg.id),
            clew_protocol::Request::WriteState { rel, text, .. } if rel == want => {
                writes.push((msg.id, text.clone()))
            }
            _ => {}
        }
    }
    (edits, writes)
}

/// `remote_state_dirty` marks two different facts, and only one of them is "an
/// edit is on its way". A transport that dies between the send and the
/// acknowledgement leaves a change the server never received; if the user then
/// edits the SAME file inside the reconnect window, that rel is in flight
/// again, and the re-read's guard used to take the in-flight edit for the whole
/// story: it skipped the flush that rescues the lost change, and the merged
/// reply — computed without it — was adopted over this window's copy. The
/// change disappeared from the remote file and from this window's list at once,
/// silently, after the user had been told it was saved.
#[test]
fn an_edit_in_the_reconnect_window_does_not_discard_an_unacknowledged_one() {
    let (mut app, root_s, mut rx) = window_with_an_unsent_removal("remote-state-unsent");

    // Inside the reconnect window — before the re-read's content lands — the
    // user removes a second bookmark. THIS one reaches the server.
    let _ = app.on_bookmark_removed(0);
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(seen, ["c.rs"], "both removals are applied optimistically");

    // The re-read lands: the file as the server has it, with NEITHER removal.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s.clone(),
        rel: "bookmarks.json".into(),
        text: Some(THREE_BOOKMARKS.into()),
    });
    let (edits, mut writes) = took_state_writes(&mut rx, "bookmarks.json");
    assert_eq!(
        edits.len(),
        1,
        "the reconnect-window removal went out as a merge"
    );
    let (_, flushed) = writes
        .pop()
        .expect("the change the dead transport ate must be flushed back");
    let flushed = flushed.expect("the flush carries a file, not a deletion");
    assert!(
        !flushed.contains("a.rs") && !flushed.contains("b.rs") && flushed.contains("c.rs"),
        "the flush carries this window's copy, which holds BOTH removals: {flushed}"
    );

    // The merge for the second removal arrives. It was computed on the server's
    // file, so it still has the first removal's entry — and the flush above has
    // superseded it, so it cannot be adopted over this window's copy.
    let _ = app.handle_server_reply(
        edits[0],
        clew_protocol::Event::StateEdited {
            root: root_s,
            rel: "bookmarks.json".into(),
            text: Some(
                r#"[{"rel":"a.rs","line":1,"preview":"a"},{"rel":"c.rs","line":3,"preview":"c"}]"#
                    .into(),
            ),
        },
    );
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(
        seen,
        ["c.rs"],
        "a removal the user was told was saved must not come back from the dead"
    );
}

/// The same window with the two replies in the other order: the merge for the
/// reconnect-window edit overtakes the re-read it was sent behind. It is the
/// same file computed without the lost change, so adopting it is the same
/// erasure — and clearing the mark with it would leave the flush that follows
/// nothing to carry.
#[test]
fn a_merge_that_overtakes_the_reconnect_read_is_not_adopted_over_a_lost_change() {
    let (mut app, root_s, mut rx) = window_with_an_unsent_removal("remote-state-unsent-reorder");
    let _ = app.on_bookmark_removed(0);
    let (edits, _) = took_state_writes(&mut rx, "bookmarks.json");
    assert_eq!(edits.len(), 1);

    // The merge lands FIRST, carrying only the removal the server saw.
    let _ = app.handle_server_reply(
        edits[0],
        clew_protocol::Event::StateEdited {
            root: root_s.clone(),
            rel: "bookmarks.json".into(),
            text: Some(
                r#"[{"rel":"a.rs","line":1,"preview":"a"},{"rel":"c.rs","line":3,"preview":"c"}]"#
                    .into(),
            ),
        },
    );
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(
        seen,
        ["c.rs"],
        "the merge must not resurrect the lost removal"
    );
    assert!(
        app.remote_state_dirty.contains("bookmarks.json")
            && app.remote_state_unsent.contains("bookmarks.json"),
        "and the file is still short of that removal, so both marks stand"
    );

    // Then the re-read, which is what carries the change back to the remote.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s,
        rel: "bookmarks.json".into(),
        text: Some(THREE_BOOKMARKS.into()),
    });
    let (_, mut writes) = took_state_writes(&mut rx, "bookmarks.json");
    let (_, flushed) = writes
        .pop()
        .expect("the re-read must flush this window's copy");
    let flushed = flushed.expect("the flush carries a file, not a deletion");
    assert!(
        !flushed.contains("a.rs") && !flushed.contains("b.rs") && flushed.contains("c.rs"),
        "which holds both removals: {flushed}"
    );
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(seen, ["c.rs"]);
}

/// The third order: the re-read's rescue flush goes out FIRST, and the user
/// edits the same store again inside that one round trip. The edit supersedes
/// the flush's id (it owns the dirty mark from then on), so nothing but the
/// flush's OWN acknowledgement can say the lost change is on the remote's disk
/// again. Reading that off the superseded id left both marks set forever: every
/// later merge for this store was dropped without adopting — another client's
/// entries stopped appearing — and every reconnect rewrote this window's
/// ever-staler copy wholesale over the remote file.
#[test]
fn a_rescue_flush_superseded_by_a_later_edit_still_retires_the_unsent_mark() {
    let (mut app, root_s, mut rx) = window_with_an_unsent_removal("remote-state-unsent-superseded");

    // The re-read lands first, so THIS is what flushes the lost removal.
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root_s.clone(),
        rel: "bookmarks.json".into(),
        text: Some(THREE_BOOKMARKS.into()),
    });
    let (edits, mut writes) = took_state_writes(&mut rx, "bookmarks.json");
    assert!(edits.is_empty(), "no edit has been made in this window yet");
    let (write_id, flushed) = writes.pop().expect("the re-read flushes the lost removal");
    let flushed = flushed.expect("the flush carries a file, not a deletion");
    assert!(
        !flushed.contains("a.rs") && flushed.contains("b.rs") && flushed.contains("c.rs"),
        "the flush carries this window's copy: {flushed}"
    );

    // Inside that round trip the user removes another bookmark. It supersedes
    // the flush's id in the in-flight table, because from here IT owns the
    // dirty mark — the flush's bytes do not contain this removal.
    let _ = app.on_bookmark_removed(0);
    let (edits, _) = took_state_writes(&mut rx, "bookmarks.json");
    assert_eq!(edits.len(), 1, "the second removal went out as a merge");

    // The server's state worker is ordered, so the flush is acknowledged
    // first. Its bytes are on the disk, so the lost removal is no longer lost.
    let _ = app.handle_server_reply(
        write_id,
        clew_protocol::Event::StateWritten {
            root: root_s.clone(),
            rel: "bookmarks.json".into(),
        },
    );
    assert!(
        !app.remote_state_unsent.contains("bookmarks.json"),
        "the flush that carried the lost removal was acknowledged: it is on the remote's disk"
    );
    assert!(
        app.remote_state_dirty.contains("bookmarks.json"),
        "but the newer removal is still only in this window, and owns the plain mark"
    );

    // Then the merge for the second removal, computed by the server on the
    // file the flush had just written — plus a bookmark another client added.
    let _ = app.handle_server_reply(
        edits[0],
        clew_protocol::Event::StateEdited {
            root: root_s,
            rel: "bookmarks.json".into(),
            text: Some(
                r#"[{"rel":"c.rs","line":3,"preview":"c"},{"rel":"d.rs","line":4,"preview":"theirs"}]"#
                    .into(),
            ),
        },
    );
    let seen: Vec<_> = app.bookmarks.iter().map(|b| b.rel.as_str()).collect();
    assert_eq!(
        seen,
        ["c.rs", "d.rs"],
        "the merged file is adopted, so the other client's entry appears"
    );
    assert!(
        app.remote_state_dirty.is_empty() && app.remote_state_unsent.is_empty(),
        "and both marks are retired: every change this window made is on the remote's disk"
    );
}

/// The settings form must not pre-fill a key that came from the environment.
///
/// Saving the form writes whatever is in the field into `config.toml`, and a
/// STORED key follows `base_url` wherever the user points it — so a pre-filled
/// form let someone with `ANTHROPIC_API_KEY` exported open Settings, type a
/// gateway URL, save, and hand that gateway their real provider secret. The
/// endpoint check that keeps an environment key on its own provider's endpoint
/// cannot see that route, because by then the key is stored.
#[test]
fn settings_never_prefill_a_key_that_came_from_the_environment() {
    let data = std::env::temp_dir().join("clew-settings-env-key");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // Takes the env lock and holds it until the guard drops, so the two key
    // variables below are serialized against the other env-touching tests too.
    let _env = data_dir_override(&data);
    // SAFETY: env mutation serialized by env_lock, held by `_env`.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "env-secret");
        std::env::set_var("OPENAI_API_KEY", "env-embed-secret");
    }

    // No config.toml at all: both keys can only have come from the environment.
    let mut app = blank_app();
    let _ = app.update(Message::OpenSettings);
    assert!(app.settings.open);
    assert_eq!(
        app.settings.key, "",
        "the chat key field must stay blank, not carry env-secret into the form"
    );
    assert!(
        app.settings.key_from_env,
        "the form has to be able to say why the field is blank"
    );
    assert_eq!(app.settings.embed_key, "", "same for the embedding key");
    assert!(app.settings.embed_key_from_env);

    // A key the user actually stored is theirs, and still pre-fills.
    let mut llm = toml::Table::new();
    llm.insert("provider".into(), "anthropic".into());
    llm.insert("api_key".into(), "typed-by-the-user".into());
    clew_core::globalconfig::update("llm", llm).unwrap();
    let mut app = blank_app();
    let _ = app.update(Message::OpenSettings);
    assert_eq!(app.settings.key, "typed-by-the-user");
    assert!(
        !app.settings.key_from_env,
        "a stored key is not an environment key"
    );

    // `CLEW_DATA_DIR` is restored by the guard; these two have no suite-wide
    // default to go back to and no other test in this binary reads them.
    // SAFETY: env mutation serialized by env_lock, still held by `_env`.
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
    }
}

/// The gutter must not draw a breakpoint the adapter refused the same way it
/// draws one that will fire. Both call sites used to `let _ =` the adapter's
/// answer away, so a breakpoint on a blank line, in optimized-out code, or in a
/// file whose path does not match the binary's debug info showed a confident
/// red dot and never fired.
#[test]
fn a_refused_breakpoint_is_recorded_rather_than_discarded() {
    use crate::app::model::Bp;
    use serde_json::json;

    let mut app = blank_app();
    let file = PathBuf::from("/p/src/lib.rs");
    let lines = app.debug.breakpoints.entry(file.clone()).or_default();
    lines.insert(10, Bp::default());
    lines.insert(20, Bp::default());
    // Conditional, to pin down that the teardown below forgets the adapter's
    // answers without touching what the user typed.
    lines.insert(
        30,
        Bp {
            condition: Some("i == 3".into()),
            ..Bp::default()
        },
    );
    // Nothing is known before the adapter answers, and "unknown" is not
    // "refused": the gutter keeps drawing those solid.
    assert!(
        app.debug.breakpoints[&file]
            .values()
            .all(|b| b.verified.is_none())
    );

    let answers = vec![(
        file.clone(),
        Ok(vec![
            // Bound where asked.
            dap::Breakpoint::from_value(10, &json!({ "id": 1, "verified": true, "line": 10 })),
            // Refused, with the adapter's reason.
            dap::Breakpoint::from_value(
                20,
                &json!({ "id": 2, "verified": false, "message": "no code at this line" }),
            ),
            // Bound, but a line further down.
            dap::Breakpoint::from_value(30, &json!({ "id": 3, "verified": true, "line": 34 })),
        ]),
    )];
    let _ = app.update(Message::DapBreakpointsAnswered {
        run: app.debug_run,
        answers,
    });

    let bps = &app.debug.breakpoints[&file];
    assert_eq!(bps[&10].verified, Some(true));
    assert_eq!(bps[&10].bound_line, None, "it stayed put");
    assert_eq!(bps[&20].verified, Some(false), "the refusal must survive");
    assert_eq!(bps[&30].verified, Some(true));
    assert_eq!(bps[&30].bound_line, Some(34), "the adapter moved it");
    assert!(
        app.status.contains("could not be set"),
        "the refusal is reported: {}",
        app.status
    );
    // The handles are kept so a later `breakpoint` event can find these lines.
    assert_eq!(bps[&20].adapter_id, Some(2));

    // A reply from a session the user already stopped must not relabel the
    // breakpoints of the next one.
    let stale = app.debug_run;
    app.bump_debug_run();
    let _ = app.update(Message::DapBreakpointsAnswered {
        run: stale,
        answers: vec![(
            file.clone(),
            Ok(vec![dap::Breakpoint::from_value(
                10,
                &json!({ "verified": false }),
            )]),
        )],
    });
    assert_eq!(
        app.debug.breakpoints[&file][&10].verified,
        Some(true),
        "a reply from the previous run must be dropped"
    );

    // Stop ends the adapter, and with it its standing to say anything about
    // these lines. Keeping `Some(false)` would go on drawing line 20 hollow —
    // "this will never fire" — with no adapter alive to claim it, and through a
    // later start that dies before it ever reaches `setBreakpoints`. The handle
    // has to go too: the next adapter hands out ids from zero, and a stale `2`
    // would let its `breakpoint` event relabel this line.
    let _ = app.update(Message::DebugStop);
    let bps = &app.debug.breakpoints[&file];
    assert!(
        bps.values().all(|b| b.verified.is_none()),
        "the dead adapter's verdicts are forgotten at Stop"
    );
    assert!(bps.values().all(|b| b.bound_line.is_none()));
    assert!(bps.values().all(|b| b.adapter_id.is_none()));
    assert_eq!(bps.len(), 3, "the breakpoints themselves survive the stop");
    assert_eq!(
        bps[&30].condition.as_deref(),
        Some("i == 3"),
        "the condition is the user's input, not the adapter's verdict"
    );
}

/// Adapters bind lazily: a breakpoint in a module that is not loaded yet comes
/// back unverified and flips to true later, over the `breakpoint` EVENT and
/// with no second request from us. That event used to be parsed as
/// `DapEvent::Other` and dropped, so the gutter kept the first answer forever.
#[test]
fn a_late_breakpoint_event_revises_the_first_answer() {
    use crate::app::model::Bp;
    use serde_json::json;

    let mut app = blank_app();
    let file = PathBuf::from("/p/src/lib.rs");
    app.debug
        .breakpoints
        .entry(file.clone())
        .or_default()
        .insert(7, Bp::default());
    let _ = app.update(Message::DapBreakpointsAnswered {
        run: app.debug_run,
        answers: vec![(
            file.clone(),
            Ok(vec![dap::Breakpoint::from_value(
                7,
                &json!({ "id": 99, "verified": false }),
            )]),
        )],
    });
    assert_eq!(app.debug.breakpoints[&file][&7].verified, Some(false));

    let event = dap::DapEvent::parse(
        "breakpoint",
        &json!({ "reason": "changed", "breakpoint": { "id": 99, "verified": true, "line": 7 } }),
    );
    assert!(
        matches!(event, dap::DapEvent::BreakpointChanged { id: Some(99), .. }),
        "the event has to be parsed at all"
    );
    let _ = app.update(Message::DapEvent {
        run: app.debug_run,
        event,
    });
    assert_eq!(
        app.debug.breakpoints[&file][&7].verified,
        Some(true),
        "the module loaded; the breakpoint is live now"
    );

    // An event for a handle we never set must not touch anybody else's line.
    let _ = app.update(Message::DapEvent {
        run: app.debug_run,
        event: dap::DapEvent::parse(
            "breakpoint",
            &json!({ "reason": "changed", "breakpoint": { "id": 1234, "verified": false } }),
        ),
    });
    assert_eq!(app.debug.breakpoints[&file][&7].verified, Some(true));
}

/// A file changed on the REMOTE host must still reach the language server.
/// The client owns the LSP document overlay on both connection types, and the
/// server keeps its copy from `didOpen` until a `didClose` clew never sends —
/// but the remote branch of the watcher event only ever re-read files that a
/// pane happened to show, and applying that reply sent no `didChange`. Every
/// definition, reference, hover and inlay hint for the file then resolved
/// against the text as it was when the file was first opened, for the rest of
/// the session. The re-read must also cover a file the language server still
/// holds while the pane has moved on, and it must come from the server: the
/// same absolute path on THIS disk is another machine's data.
#[tokio::test]
async fn a_remote_change_still_reaches_the_language_server() {
    let mut app = scanned_app("remote-lsp-resync");
    let root = app.project.as_ref().unwrap().root.clone();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    let (client, wire) = recording_lsp_client(&root).await;
    app.lsp.insert("rust".into(), LspSlot::Ready(client));

    // src/lib.rs was opened earlier this session, so the server holds it; the
    // pane has since moved on to another file.
    let lib = root.join("src/lib.rs");
    app.lsp_opened.insert(lib.clone());
    open_synchronously(&mut app, "notes.txt", None);
    assert!(
        app.panes.iter().flatten().all(|v| v.abs != lib),
        "the changed file must be off screen for this to test anything"
    );
    // A local copy that must never be read: the truth is on the remote host.
    std::fs::write(&lib, "pub fn local_secret() {}\n").unwrap();

    let _ = app.handle_server_event(clew_protocol::Event::FilesChanged {
        root: root.to_string_lossy().into_owned(),
        rels: vec!["src/lib.rs".into()],
    });
    let mut reread = None;
    while let Ok(msg) = rx.try_recv() {
        if matches!(&msg.request, clew_protocol::Request::ReadFile { rel, .. } if rel == "src/lib.rs")
        {
            reread = Some(msg.id);
        }
    }
    let id = reread.expect("a file the language server holds must be re-read from the server");

    // The server answers with the bytes as they are over there.
    let edited = "pub fn origin() {}\n\npub fn edited_on_the_remote() {}\n";
    let _ = app.handle_server_reply(
        id,
        clew_protocol::Event::FileContent {
            rel: "src/lib.rs".into(),
            source: edited.into(),
            lines: Vec::new(),
            symbols: Vec::new(),
            docs: Vec::new(),
            inactive: Vec::new(),
        },
    );

    let seen = wire_containing(&wire, "textDocument/didChange").await;
    assert!(
        seen.contains("edited_on_the_remote"),
        "the didChange must carry the remote's post-change text: {seen:?}"
    );
    assert!(
        !seen.contains("local_secret"),
        "the resync read this machine's disk instead of the server: {seen:?}"
    );
}

/// A remote change refreshes the pane's text, so the git view it was drawn
/// against is gone: `GitInfo`'s `blame` and `status` are indexed by 0-based
/// line, and nothing else re-requests them on a remote project. Left alone,
/// one insertion above shifts every gutter bar and makes the caret-line blame
/// name a plausible but wrong commit. The stale vectors must be dropped as the
/// text is replaced, and a fresh pass asked for.
#[test]
fn a_remote_refresh_drops_the_stale_git_view_and_asks_again() {
    let mut app = scanned_app("remote-git-refresh");
    let root = app.project.as_ref().unwrap().root.clone();
    // Open while local so the pane is built from this fixture's bytes, then
    // become the remote client whose refreshes come over the protocol.
    open_synchronously(&mut app, "src/lib.rs", None);
    let abs = root.join("src/lib.rs");
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    for v in app.panes.iter_mut().flatten().filter(|v| v.abs == abs) {
        v.git = Some(Arc::new(clew_protocol::GitInfo {
            blame: Vec::new(),
            status: vec![Some(clew_protocol::ChangeKind::Modified)],
            deleted_at: Default::default(),
        }));
    }

    let _ = app.apply_file_refresh(
        "src/lib.rs".into(),
        "pub fn inserted_above() {}\npub fn origin() {}\n".into(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );

    assert!(
        app.panes
            .iter()
            .flatten()
            .filter(|v| v.abs == abs)
            .all(|v| v.git.is_none()),
        "a per-line git view of bytes that are gone must not stay on screen"
    );
    let mut asked = false;
    while let Ok(msg) = rx.try_recv() {
        asked |=
            matches!(&msg.request, clew_protocol::Request::GitInfo { rel } if rel == "src/lib.rs");
    }
    assert!(
        asked,
        "the refreshed text must get a blame pass for its own revision"
    );
}

/// Two git passes for one file are not ordered against each other, and the
/// loser is simply whichever is applied last. A blame requested from the
/// server against the pre-change bytes must therefore be retired when the
/// watcher starts a newer local pass, or it repaints the gutter and the
/// caret-line blame from the previous revision with no way back.
#[test]
fn a_superseded_blame_reply_cannot_repaint_the_gutter() {
    let mut app = scanned_app("stale-blame-order");
    let root = app.project.as_ref().unwrap().root.clone();
    open_synchronously(&mut app, "src/lib.rs", None);
    let abs = root.join("src/lib.rs");

    // A blame for the pre-change bytes is still running on the server.
    app.pending_git.insert(77, abs.clone());

    // The file changes on disk; the watcher's rehash reloads the pane and
    // starts a git pass over the NEW bytes.
    let edited = "pub fn inserted_above() {}\npub fn origin() {}\n";
    std::fs::write(&abs, edited).unwrap();
    let _ = app.update(rehashed(&app, vec![modified(&abs, 4242, edited)]));

    // That newer pass lands first.
    let fresh = Arc::new(clew_protocol::GitInfo {
        blame: Vec::new(),
        status: vec![Some(clew_protocol::ChangeKind::Added)],
        deleted_at: Default::default(),
    });
    let _ = app.on_git_info_loaded(abs.clone(), Some(fresh.clone()));

    // …and only then does the older server reply arrive.
    let _ = app.handle_server_reply(
        77,
        clew_protocol::Event::GitInfo {
            rel: "src/lib.rs".into(),
            info: Some(clew_protocol::GitInfo::default()),
        },
    );

    let painted = app
        .panes
        .iter()
        .flatten()
        .find(|v| v.abs == abs)
        .and_then(|v| v.git.clone())
        .expect("the newer pass painted the gutter");
    assert!(
        Arc::ptr_eq(&painted, &fresh),
        "a blame for the pre-change bytes repainted the gutter after a newer one"
    );
}

/// One `Event::Docs` reply carrying `origin` at `line`, as the server's
/// `BuildDocs` answers it.
fn docs_reply(root: &Path, line: usize) -> clew_protocol::Event {
    clew_protocol::Event::Docs {
        root: root.to_string_lossy().into_owned(),
        files: vec![clew_protocol::DocFile {
            rel: "src/lib.rs".into(),
            items: vec![clew_protocol::DocItem {
                name: "origin".into(),
                kind: "function".into(),
                signature: format!("pub fn origin() -> Point // line {line}"),
                doc: String::new(),
                line,
                public: true,
                children: Vec::new(),
            }],
        }],
    }
}

/// Take the next `BuildDocs` sitting in the client's outbox, if any.
fn took_build_docs(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<clew_protocol::ClientMessage>,
) -> bool {
    let mut found = false;
    while let Ok(msg) = rx.try_recv() {
        found |= matches!(msg.request, clew_protocol::Request::BuildDocs);
    }
    found
}

/// An edit made while DOCS is NOT the visible sidebar tab must not leave the
/// API surface pinned to the pre-edit project. The only automatic rebuild fires
/// on `FilesChanged` while DOCS is visible, and re-entering the tab used to
/// rebuild only when the list was EMPTY — so a non-empty index built before the
/// edit was served as current for the rest of the session: old signatures, and
/// an "Open source" button carrying a line the edit had moved. The index now
/// carries the change-registry revision it was built at, the same freshness key
/// Stats and Project Calls compare.
#[test]
fn an_edit_made_off_the_docs_tab_rebuilds_the_index_when_the_tab_returns() {
    let mut app = scanned_app("docs-stale-local");
    let root = app.project.as_ref().unwrap().root.clone();
    let abs = root.join("src/lib.rs");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    // Seed the registry so the edit below reads as a content change rather than
    // a file creation (which would take the structural rescan path instead).
    app.registry.set(abs.clone(), 111);

    // The reader opens DOCS: a build goes out and its answer lands.
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Docs));
    assert!(
        app.docs.loading && took_build_docs(&mut rx),
        "opening DOCS on an empty index must request a build"
    );
    let _ = app.handle_server_event(docs_reply(&root, 3));
    assert!(!app.docs.files.is_empty() && !app.docs.loading);
    // …and reads one item's page.
    app.open_doc_page("src/lib.rs", 3);
    assert_eq!(app.docs.page.as_ref().unwrap().entries[0].line, 3);

    // They leave for another tab, and `origin` moves down a line on disk.
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Files));
    let src = "// a note\npub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n";
    std::fs::write(&abs, src).unwrap();
    let _ = app.update(rehashed(&app, vec![modified(&abs, 222, src)]));
    assert!(
        !app.docs.loading && !took_build_docs(&mut rx),
        "a hidden DOCS tab is not rebuilt on the change itself"
    );
    assert!(
        !app.docs_fresh(),
        "the index predates the edit, so it must not read as fresh"
    );

    // Coming back to DOCS must rebuild rather than serve the pre-edit index.
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Docs));
    assert!(
        app.docs.loading && took_build_docs(&mut rx),
        "re-entering DOCS after an edit must rebuild the index"
    );

    // And the answer re-points the page the reader still has open: it was
    // flattened from the previous index, so its entries (and the line its
    // "Open source" button presses) would otherwise stay pre-edit.
    let _ = app.handle_server_event(docs_reply(&root, 4));
    let page = app.docs.page.as_ref().expect("the open doc page");
    assert_eq!(
        page.entries[0].line, 4,
        "the open doc page kept the pre-edit line after a rebuild"
    );
    assert!(
        page.entries[0].signature.contains("line 4"),
        "the open doc page kept the pre-edit signature after a rebuild"
    );
    assert!(app.docs_fresh(), "the rebuilt index must read as fresh");
}

/// The same contract on a REMOTE project, where this client never sees a file's
/// bytes: the registry is advanced by the server's `ProjectSymbols`
/// publications, so the docs index keys off exactly the same revision there.
#[test]
fn a_remote_edit_off_the_docs_tab_rebuilds_the_index_when_the_tab_returns() {
    let mut app = scanned_app("docs-stale-remote");
    let root = app.project.as_ref().unwrap().root.clone();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    let publish = |app: &mut App, seq: u64, full: bool, line: usize| {
        let _ = app.handle_server_event(clew_protocol::Event::ProjectSymbols {
            root: root.to_string_lossy().into_owned(),
            seq,
            full,
            files: vec![clew_protocol::FileSymbols {
                rel: "src/lib.rs".into(),
                symbols: vec![clew_protocol::IndexSymbol {
                    name: "origin".into(),
                    kind: "function".into(),
                    line,
                    is_test: false,
                }],
                imports: Vec::new(),
            }],
            go_module: clew_protocol::Patch::Unchanged,
            dart_package: clew_protocol::Patch::Unchanged,
            structure: clew_protocol::Patch::Unchanged,
        });
    };
    publish(&mut app, 1, true, 3);

    // Built while the reader is on DOCS.
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Docs));
    assert!(app.docs.loading && took_build_docs(&mut rx));
    let _ = app.handle_server_event(docs_reply(&root, 3));
    assert!(app.docs_fresh());

    // They leave the tab; the remote edit reaches this client only as a partial
    // publication (nothing here ever reads the file).
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Files));
    publish(&mut app, 2, false, 9);
    let _ = app.handle_server_event(clew_protocol::Event::FilesChanged {
        root: root.to_string_lossy().into_owned(),
        rels: vec!["src/lib.rs".into()],
    });
    assert!(
        !took_build_docs(&mut rx),
        "a hidden DOCS tab is not rebuilt on the change itself"
    );
    assert!(!app.docs_fresh(), "a remote edit must age the docs index");

    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Docs));
    assert!(
        app.docs.loading && took_build_docs(&mut rx),
        "re-entering DOCS after a remote edit must rebuild the index"
    );
}

/// A `BuildDocs` that is abandoned unanswered (the transport died, or the server
/// refused it) must not leave its request-time revision stamped on the index it
/// was going to replace — that would label the PREVIOUS index as current for the
/// revision it does not describe, the one over-claim this key must never make.
#[test]
fn an_abandoned_docs_build_leaves_the_index_reading_stale() {
    let mut app = scanned_app("docs-abandoned-build");
    let root = app.project.as_ref().unwrap().root.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Docs));
    assert!(took_build_docs(&mut rx));
    let _ = app.handle_server_event(docs_reply(&root, 3));

    // A change ages it, and the rebuild it triggers is refused by the server.
    app.registry.set(root.join("src/lib.rs"), 222);
    app.ensure_docs();
    let id = app.pending_docs.expect("the rebuild request id");
    let _ = app.handle_server_reply(
        id,
        clew_protocol::Event::Error {
            message: "docs build failed".into(),
        },
    );
    assert!(!app.docs.loading, "a refused build must clear the spinner");
    assert!(
        !app.docs_fresh(),
        "a refused build must leave the pre-edit index reading stale"
    );

    // Dropping the transport mid-build is the same story with no reply at all.
    app.ensure_docs();
    app.drop_connection_state();
    assert!(
        !app.docs_fresh(),
        "an unanswerable build must leave the index reading stale"
    );
}

/// A "View docs" on an index that isn't current parks the symbol name on the
/// build it asks for, and `Event::Docs` is the ONLY consumer of that name. A
/// build that ends WITHOUT one — refused by the server, or abandoned when the
/// transport died — therefore left the name aimed at the next SUCCESSFUL build
/// of the same project: long after this client had reported the failure, an
/// unrelated rebuild (a DOCS tab visit, an edit landing on the tab) opened that
/// symbol's page over the file the reader was on, clearing Overview and Stats
/// with it. The name is released wherever the build it waits on is reaped.
#[test]
fn an_abandoned_docs_build_does_not_hijack_the_pane_later() {
    let mut app = scanned_app("docs-abandoned-view");
    let root = app.project.as_ref().unwrap().root.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);

    // "View docs" with nothing built: the name rides the build that goes out.
    app.view_docs_for("origin");
    assert!(app.docs.loading && took_build_docs(&mut rx));
    assert_eq!(app.docs.pending_view.as_deref(), Some("origin"));

    // The transport dies before the build answers.
    app.drop_connection_state();
    assert_eq!(
        app.docs.pending_view, None,
        "a build that can never answer must not keep the parked name alive"
    );

    // A new transport, and later an ordinary rebuild that DOES answer.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    app.ensure_docs();
    assert!(took_build_docs(&mut rx));
    let _ = app.handle_server_event(docs_reply(&root, 3));
    assert!(
        app.docs.page.is_none(),
        "a View docs the client reported as failed must not fire against a later build"
    );

    // The server's own refusal is the same story: an edit ages the index, the
    // "View docs" parks on the rebuild, and the rebuild comes back an Error.
    app.registry.set(root.join("src/lib.rs"), 222);
    app.view_docs_for("origin");
    assert_eq!(app.docs.pending_view.as_deref(), Some("origin"));
    let id = app.pending_docs.expect("the rebuild request id");
    let _ = app.handle_server_reply(
        id,
        clew_protocol::Event::Error {
            message: "docs build failed".into(),
        },
    );
    assert_eq!(
        app.docs.pending_view, None,
        "a refused build must release the name it was carrying"
    );
    let _ = app.handle_server_event(docs_reply(&root, 7));
    assert!(
        app.docs.page.is_none(),
        "and the next successful build must leave the reading pane alone"
    );
}

/// The server answers `BuildDocs` with an unsolicited `Event::Docs` carrying no
/// request id, so two builds in flight cannot be told apart when they land: the
/// first reply installs its OLDER files and clears the spinner while `docs.rev`
/// already holds the second request's stamp, and `docs_fresh` then reports a
/// pre-edit index as current. `ensure_docs` was single-flight; the manual
/// refresh went straight to `request_docs` and was not, so it could open that
/// window. The guard now lives in `request_docs`, where every caller gets it.
#[test]
fn a_manual_docs_refresh_cannot_open_a_second_build() {
    let mut app = scanned_app("docs-refresh-single-flight");
    let root = app.project.as_ref().unwrap().root.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.server_tx = Some(tx);
    let _ = app.update(Message::SidebarTabPicked(SidebarTab::Docs));
    assert!(took_build_docs(&mut rx), "entering DOCS builds the index");
    let _ = app.handle_server_event(docs_reply(&root, 3));
    assert!(app.docs_fresh());

    // An edit ages the index; the automatic rebuild goes out and is still in
    // flight when the reader presses Refresh.
    app.registry.set(root.join("src/lib.rs"), 222);
    app.ensure_docs();
    assert!(app.docs.loading && took_build_docs(&mut rx));
    let first = app.pending_docs.expect("the in-flight request id");

    let _ = app.update(Message::DocsRefresh);
    assert!(
        !took_build_docs(&mut rx),
        "a refresh during a build must not put a second BuildDocs on the wire"
    );
    assert_eq!(
        app.pending_docs,
        Some(first),
        "the in-flight build must keep the id its Error reply is correlated by"
    );

    // The one build lands. Its files and its stamp belong to the same request,
    // so freshness describes what is actually on screen.
    let _ = app.handle_server_event(docs_reply(&root, 4));
    assert!(!app.docs.loading);
    assert!(
        app.docs_fresh(),
        "the landed index is the one whose revision was stamped"
    );
}

/// An open note editor is anchored by a PROJECT-RELATIVE path and holds only a
/// draft — no project identity — while Save re-resolves the root at save time.
/// Left standing across a project switch it therefore wrote project A's note
/// into project B's `.clew/notes.json`, overwriting B's own note whenever the
/// two projects share a `(rel, symbol)` (two Rust projects both have
/// `src/lib.rs`). The switch must take the editor with it.
#[test]
fn a_note_editor_does_not_write_into_the_next_project() {
    let mut app = scanned_app("note-editor-project-a");

    // Project A: start a reading note on a symbol and type into it, without
    // saving or closing the modal.
    let _ = app.update(Message::NoteEditStart {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
    });
    let _ = app.update(Message::NoteEditInput("A's private reading note".into()));
    assert!(app.reading_note_edit.is_some(), "the editor is open");
    // The bookmark twin of the same editor, whose Save silently drops the
    // draft when no bookmark in the new project matches the old anchor.
    app.note_edit = Some(("src/lib.rs".into(), 3, "A's bookmark note".into()));

    // Project B, opened from under the modal (⌘O / File → Open Folder is a
    // NATIVE menu item, so no iced overlay blocks it), with a note of its own
    // at the very same anchor.
    let b_root = fixture_project("note-editor-project-b");
    notes::save(
        &b_root,
        &[notes::Note {
            rel: "src/lib.rs".into(),
            symbol: "origin".into(),
            understood: false,
            text: "B's own note".into(),
        }],
    )
    .unwrap();
    scan_synchronously(&mut app, b_root.clone());

    assert!(
        app.reading_note_edit.is_none(),
        "an editor anchored in the previous project must not survive the switch"
    );
    assert!(app.note_edit.is_none(), "same for the bookmark-note editor");
    assert!(
        app.status.contains("Unsaved note was discarded"),
        "prose that exists nowhere else must not vanish silently: {}",
        app.status
    );

    // Whatever Save does now, it cannot carry A's text: pressing it is a no-op.
    let _ = app.update(Message::NoteEditSave);
    let _ = app.update(Message::BookmarkNoteSave);
    let on_disk = notes::load(&b_root);
    assert_eq!(
        on_disk,
        vec![notes::Note {
            rel: "src/lib.rs".into(),
            symbol: "origin".into(),
            understood: false,
            text: "B's own note".into(),
        }],
        "project B's store must be untouched by project A's editor"
    );
    assert_eq!(app.notes, on_disk, "and the in-memory list agrees with it");
}

/// Every editor, popup and match set that anchors to a path, a pane or a trail
/// node of the project being left. None of them is rebuilt by opening a
/// project, so each one used to survive the switch and then act on the new one
/// (see the block in `on_scan_done`). The time-travel case is the loudest: its
/// key guard swallows every non-command key, so the new project's editor was
/// dead to the keyboard until something opened a file.
#[test]
fn a_project_switch_drops_every_anchor_into_the_old_project() {
    let mut app = scanned_app("switch-anchors-a");
    let a_root = app.project.as_ref().unwrap().root.clone();
    let a_file = a_root.join("src/lib.rs");

    app.debug.bp_cond_edit = Some((a_file.clone(), 3, "x > 1".into()));
    app.time_travel = Some(TimeTravel {
        abs: a_file.clone(),
        rel: "src/lib.rs".into(),
        lang: Some("rust"),
        scope: TimeScope::File,
        commits: Vec::new(),
        idx: 0,
        viewer: None,
        scroll_y: 0.0,
        caret: None,
        focus_line: None,
        loading: false,
        generation: app.time_gen,
        why: HashMap::new(),
        why_loading: false,
        story: None,
        story_loading: false,
    });
    let time_gen = app.time_gen;
    app.diff = Some(DiffState {
        abs: a_file.clone(),
        rel: "src/lib.rs".into(),
        lines: Vec::new(),
    });
    app.blame_why = Some(BlameWhy {
        token: app.blame_why_seq,
        title: "Why line 3 exists".into(),
        commits: Vec::new(),
        loading: true,
        prepared: Vec::new(),
    });
    app.context_menu = Some(ContextMenu {
        pane: 0,
        line: 3,
        col: 4,
        x: 10.0,
        y: 10.0,
    });
    app.find.open = true;
    app.find.query = "Point".into();
    app.find.matches = vec![(0, 11, 16)];
    app.trail_collapsed.insert(7);
    // A walkthrough generation whose result the epoch guard will discard.
    app.walk.generating = Some(String::new());
    app.walk.retried = true;
    // The module map is laid out from the OLD import graph. Project B has no
    // cached overview, so the recompute at the end of `on_scan_done` does not
    // run and only an explicit reset retires it.
    app.overview.map = Some(graphlayout::Layout::default());

    scan_synchronously(&mut app, fixture_project("switch-anchors-b"));

    assert!(
        app.debug.bp_cond_edit.is_none(),
        "breakpoint-condition edit"
    );
    assert!(app.time_travel.is_none(), "time-travel session");
    assert!(
        app.time_gen > time_gen,
        "a time-travel load still in flight must be invalidated too"
    );
    assert!(app.diff.is_none(), "diff against the old project's HEAD");
    assert!(app.blame_why.is_none(), "\"why is this here\" popup");
    assert!(app.context_menu.is_none(), "right-click menu");
    assert!(
        app.find.matches.is_empty() && !app.find.open && app.find.query.is_empty(),
        "in-file find matches are ranges in the old file"
    );
    assert!(
        app.trail_collapsed.is_empty(),
        "collapsed trail nodes index the history that was just replaced"
    );
    assert!(
        app.overview.map.is_none(),
        "the module map belongs to the import graph it was laid out from"
    );
    assert!(
        app.walk.generating.is_none() && !app.walk.retried,
        "the walkthrough result is dropped by the epoch guard, so nothing else \
         would ever clear the busy flag"
    );

    // And the keyboard reaches the new project. `on_key` returns early while a
    // session stands, ahead of every plain-key arm, so with one stranded the
    // finder opened (a ⌘ chord, dispatched earlier) but could not be driven.
    // Checked before any file is opened, since `open_file` clears the session
    // itself and would hide the bug.
    let _ = app.update(Message::FinderOpened(FinderMode::Files));
    assert!(
        app.finder.results.len() > 1,
        "need two entries for the selection to move"
    );
    let _ = app.update(Message::KeyPressed(
        keyboard::Key::Named(keyboard::key::Named::ArrowDown),
        keyboard::Modifiers::default(),
    ));
    assert_eq!(
        app.finder.selected, 1,
        "a stale time-travel session was swallowing the new project's keys"
    );
}

/// The transport boundary owes the same resets as the project boundary — that
/// is what `drop_project_work` is for, and the flags below were only ever
/// cleared on the `on_scan_done` side. `connect_to` calls it with no scan to
/// follow, so a switch left the new session wearing the old project's spinners
/// and accepting its navigation results.
#[test]
fn a_transport_switch_clears_the_old_projects_busy_flags() {
    let mut app = scanned_app("transport-switch-flags");
    app.walk.generating = Some("src/lib.rs".into());
    app.walk.retried = true;
    app.searching_semantic = true;
    app.explain.failed = 3;
    let (goto_seq, search_seq) = (app.goto_seq, app.search_seq);

    let _ = app.connect_to(crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    });

    assert!(
        app.walk.generating.is_none() && !app.walk.retried,
        "a walkthrough generation cannot survive the transport it was asked over"
    );
    assert!(!app.searching_semantic, "semantic-search spinner");
    assert_eq!(app.explain.failed, 0, "the old pass's error count");
    assert!(
        app.goto_seq > goto_seq && app.search_seq > search_seq,
        "in-flight definition / reference results are guarded only by these, \
         and their LSP tasks hold clones that outlive `lsp.clear()`"
    );
}

/// The periodic update check fires on a timer, so it lands mid-download as a
/// matter of course. Clearing the phase there re-armed the install button under
/// a running download, and a second press started a second download of the same
/// image. The orphan is harmless (its completion is dropped by generation), but
/// the work is wasted, so neither the check nor a second press may restart it.
#[test]
fn an_update_check_during_a_download_does_not_rearm_the_install() {
    use clew_core::update::{Release, Version};

    let mut app = blank_app();
    app.update.available = Some(AvailableUpdate {
        version: Version {
            major: 9,
            minor: 0,
            patch: 0,
        },
        dmg_url: Some("https://example.invalid/clew.dmg".into()),
        notes: Vec::new(),
    });
    app.update.phase = UpdatePhase::Downloading;
    app.update.progress = Some((10, Some(100)));
    let generation = app.update.generation;

    // The timer's check finds the same newer release while the download runs.
    let _ = app.update(Message::UpdateChecked {
        manual: false,
        result: Ok(Release {
            version: Version {
                major: 9,
                minor: 0,
                patch: 0,
            },
            notes: String::new(),
            dmg_url: Some("https://example.invalid/clew.dmg".into()),
        }),
    });
    assert_eq!(
        app.update.phase,
        UpdatePhase::Downloading,
        "a check must not clear the phase of a running download"
    );
    assert_eq!(
        app.update.progress,
        Some((10, Some(100))),
        "nor discard its progress"
    );

    // And a press that gets through anyway starts nothing: a new download would
    // bump the generation, which is what makes the previous one an orphan.
    let _ = app.update(Message::UpdateInstallStart);
    assert_eq!(
        app.update.generation, generation,
        "a second install press during a download must not start another"
    );

    // The ordinary case is unaffected: with nothing running, a check still
    // clears a previous failure.
    app.update.phase = UpdatePhase::Failed("earlier".into());
    let _ = app.update(Message::UpdateChecked {
        manual: false,
        result: Ok(Release {
            version: Version {
                major: 9,
                minor: 0,
                patch: 0,
            },
            notes: String::new(),
            dmg_url: None,
        }),
    });
    assert_eq!(app.update.phase, UpdatePhase::Idle);
}

/// The Search sidebar has two producers, and a Find References request takes it
/// over from a text search exactly as a newer search submission would. The
/// in-process fallback is retired by `search_seq`, but the SERVER search is
/// guarded by the request id in `pending_search`, which a references request
/// used to leave standing — so its reply repainted the reference list with grep
/// hits under the "(references)" label.
#[tokio::test]
async fn a_superseded_server_search_cannot_repaint_the_reference_list() {
    let mut app = scanned_app("search-then-references");
    let root = app.project.as_ref().unwrap().root.clone();
    open_synchronously(&mut app, "src/lib.rs", None);
    app.lsp
        .insert("rust".into(), LspSlot::Ready(dead_lsp_client(&root).await));

    // A server-side text search is in flight for an earlier query.
    app.search.query = "origin".into();
    app.search.running = true;
    app.search.ran = true;
    app.pending_search = Some(42);

    // The user asks for references instead; the LSP answers first.
    let _ = app.goto_request(0, 0, 0, GotoKind::References);
    let lib = root.join("src/lib.rs");
    let _ = app.show_references(vec![lsp::client::Target {
        path: lib.clone(),
        line: 0,
        character: 0,
    }]);
    assert_eq!(app.search.hits.len(), 1, "the reference list is on screen");

    // The superseded search now answers.
    let _ = app.handle_server_reply(
        42,
        clew_protocol::Event::SearchResults {
            hits: vec![clew_protocol::SearchHit {
                rel: "notes.txt".into(),
                line: 1,
                preview: "origin".into(),
            }],
            error: None,
        },
    );

    assert_eq!(
        app.search
            .hits
            .iter()
            .map(|h| h.abs.clone())
            .collect::<Vec<_>>(),
        vec![lib],
        "a search the references superseded must not repaint the sidebar"
    );
    assert_eq!(app.search.query, "(references)");
    assert!(
        !app.search.running,
        "the retired search must not leave its spinner running: an empty or \
         failed references reply never reaches show_references, and the \
         dropped search reply can no longer clear it"
    );

    // A correlated failure for the same superseded search is dropped too.
    let _ = app.handle_server_reply(
        42,
        clew_protocol::Event::Error {
            message: "search failed".into(),
        },
    );
    assert!(
        app.search.error.is_none(),
        "a superseded search's error must not stamp over a live reference list"
    );
}

/// `find.matches` are (line, col) triples in ONE document's coordinates, and
/// the highlights, the `n/m` counter and Enter all use them raw. With the bar
/// left open, opening another file into the pane used to keep the previous
/// file's triples: rectangles painted over unrelated substrings and a caret
/// parked where the query does not occur.
#[test]
fn a_pane_that_changes_file_does_not_keep_the_old_documents_find_matches() {
    let mut app = scanned_app("find-across-files");
    open_synchronously(&mut app, "src/lib.rs", None);
    let _ = app.update(Message::FindOpened);
    let _ = app.update(Message::FindQueryChanged("Point".into()));
    assert_eq!(
        app.find.matches.len(),
        3,
        "the fixture's lib.rs mentions Point three times"
    );

    // Same pane, another file — the find bar stays open (no Esc).
    open_synchronously(&mut app, "notes.txt", None);
    assert!(
        app.find.matches.is_empty(),
        "notes.txt does not contain the query: {:?}",
        app.find.matches
    );
    // And what is left describes the file on screen.
    for &(line, c0, c1) in &app.find.matches {
        let v = app.active_viewer().unwrap();
        let text: String = v.lines[line]
            .spans
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<String>()
            .chars()
            .skip(c0)
            .take(c1 - c0)
            .collect();
        assert_eq!(text.to_lowercase(), "point");
    }

    // A query typed against the new document matches it.
    let _ = app.update(Message::FindQueryChanged("needle".into()));
    assert_eq!(app.find.matches, vec![(0, 0, 6)]);

    // The other half of a split shows a different file, and focusing it moves
    // the bar (and its highlights) there too.
    let _ = app.update(Message::ToggleSplit);
    open_synchronously(&mut app, "src/lib.rs", None); // into the now-active pane 1
    let _ = app.update(Message::FindQueryChanged("Point".into()));
    assert_eq!(app.find.matches.len(), 3);
    let _ = app.update(Message::PaneFocused(0));
    assert!(
        app.find.matches.is_empty(),
        "pane 0 still shows notes.txt: {:?}",
        app.find.matches
    );
}
