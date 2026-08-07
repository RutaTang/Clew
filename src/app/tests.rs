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
        gather_fn_detail_input(file, "parseAll", 0, &HashMap::new()).expect("detail");
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

/// Each test gets its own directory: tests run in parallel and would
/// otherwise race on remove_dir_all/create of a shared fixture.
fn fixture_project(tag: &str) -> PathBuf {
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
        lines,
        symbols,
        docs,
        inactive,
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
    let mut app = App::blank();
    scan_synchronously(&mut app, root);
    app
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
    let new = std::sync::Arc::new("pub fn renamed() -> u8 {\n    1\n}\n".to_string());
    let ev = watch::FileEvent::Modified(watch::Changed {
        path: abs.clone(),
        hash: 424242,
        content: new,
    });
    let _ = app.update(Message::FilesRehashed {
        events: vec![ev],
        fs_structural: false,
    });
    assert!(app.symbol_index.iter().any(|e| e.name == "renamed"));
    assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
    assert_eq!(app.registry.version(&abs), Some(424242));

    // Deleting the file drops its symbols and forgets its version.
    let _ = app.update(Message::FilesRehashed {
        events: vec![watch::FileEvent::Deleted(abs.clone())],
        fs_structural: false,
    });
    assert!(!app.symbol_index.iter().any(|e| e.name == "renamed"));
    assert_eq!(app.registry.version(&abs), None);
    assert!(!app.symbol_index_by_file.contains_key(&abs));
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
    let _env = clew_core::env_lock();
    let data = std::env::temp_dir().join("clew-consent-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // Consent is recorded in clew's data directory, never in the project — a
    // repository must not be able to grant itself permission.
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::set_var("CLEW_DATA_DIR", &data) };

    let root = fixture_project("consent");

    // Picking an untrusted folder opens the consent modal, not the project.
    let mut app = App::blank();
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
    let mut planted = App::blank();
    let _ = planted.update(Message::FolderPicked(Some(root.clone())));
    assert_eq!(
        planted.pending_consent.as_deref(),
        Some(root.as_path()),
        "a repo-provided .clew must not grant consent"
    );
    assert!(!planted.scanning);

    // Allowed: the scan starts and the trust record is written outside the project.
    let mut app = App::blank();
    let _ = app.update(Message::FolderPicked(Some(root.clone())));
    let _ = app.update(Message::ConsentAllowed);
    assert!(app.scanning);
    assert!(app.pending_consent.is_none());
    assert!(clew_core::trust::Trust::load().is_root_trusted(None, &root));

    // A trusted root skips the modal on the next open.
    let mut app2 = App::blank();
    let _ = app2.update(Message::FolderPicked(Some(root.clone())));
    assert!(app2.scanning, "a trusted root must skip the prompt");
    assert!(app2.pending_consent.is_none());

    unsafe { std::env::remove_var("CLEW_DATA_DIR") };
}

#[test]
fn auto_refresh_throttles_but_manual_does_not() {
    use std::time::{Duration, Instant};

    let mut app = App::blank();
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
    let _env = clew_core::env_lock();
    // Point the store at a guaranteed-empty dir so nothing is "installed".
    let store = std::env::temp_dir().join("clew-lsp-empty-store");
    let _ = std::fs::remove_dir_all(&store);
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::set_var("CLEW_DATA_DIR", &store) };

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

    unsafe { std::env::remove_var("CLEW_DATA_DIR") };
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

    let mut app = App::blank();
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
    let mut app = App::blank();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);

    // Nothing started; the user is asked, and sees the exact command line.
    assert!(!matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));
    let pending = app
        .pending_lsp_command
        .as_ref()
        .expect("a repo-specified command must be confirmed");
    assert!(pending.command_line().contains("run-lsp.sh"));
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
    let mut app = App::blank();
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
    let _env = clew_core::env_lock();
    let data = std::env::temp_dir().join("clew-lsp-switch-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::set_var("CLEW_DATA_DIR", &data) };

    let root_a = fixture_project("lsp-switch-a");
    std::fs::create_dir_all(root_a.join(".clew")).unwrap();
    std::fs::write(root_a.join("run-lsp.sh"), "#!/bin/sh\nexec ra\n").unwrap();
    std::fs::write(
        root_a.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"run-lsp.sh\"\n",
    )
    .unwrap();
    let mut app = App::blank();
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
        command: root_a.join("run-lsp.sh"),
        args: vec![],
        server_name: "rust-analyzer".into(),
        version: "x".into(),
        fingerprint: fp.clone(),
    });
    let _ = app.update(Message::LspCommandAllowed);
    assert!(
        !app.trust.is_lsp_approved(None, &root_a, "rust", &fp)
            && !app.trust.is_lsp_approved(None, &root_b, "rust", &fp),
        "approving a stale modal must record nothing"
    );
    assert!(!matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));

    unsafe { std::env::remove_var("CLEW_DATA_DIR") };
}

/// Approving the modal starts what the file contains NOW, not what it
/// contained when the modal was raised: a script swapped while the dialog sat
/// open fails the fresh fingerprint check and re-raises the modal instead of
/// running.
#[test]
fn approval_spawns_the_current_file_not_the_remembered_one() {
    let _env = clew_core::env_lock();
    let data = std::env::temp_dir().join("clew-lsp-toctou-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::set_var("CLEW_DATA_DIR", &data) };

    let root = fixture_project("lsp-toctou");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    let script = root.join("run-lsp.sh");
    std::fs::write(&script, "#!/bin/sh\nexec ra\n").unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"run-lsp.sh\"\n",
    )
    .unwrap();
    let mut app = App::blank();
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

    unsafe { std::env::remove_var("CLEW_DATA_DIR") };
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

    let mut app = App::blank();
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
    // Old watch values are dropped too.
    let _ = app.update(Message::DebugWatchesEvaluated {
        run: 1,
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
    app.pending_reads.insert(7, ReadKind::Refresh);
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

    // A deferred scan root is dropped, not scanned locally.
    app.pending_scan_root = Some(app.project.as_ref().unwrap().root.clone());
    let _ = app.update(Message::ServerUnavailable { conn: app.conn_gen });
    assert!(app.pending_scan_root.is_none());
    assert!(!app.scanning, "no local scan of a remote root");
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

    let mut app = App::blank();
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
        go_module: None,
        dart_package: None,
        structure: None,
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
    let (_, body0, _) = gather_fn_detail_input(file.clone(), "new", 0, &empty).unwrap();
    let (_, body1, _) = gather_fn_detail_input(file.clone(), "new", 1, &empty).unwrap();
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
    assert!(pending.command_line().contains("/remote/proj/run-lsp.sh"));
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

/// A repository must not be able to hand clew a symbol index. Entries used to
/// be reused on a content hash the repository can compute for its own files,
/// from a cache inside the project — so committing `.clew/cache/index.json`
/// forged clew's own conclusions: navigation, both graphs, and the source it
/// hands to the model, with no code execution at all.
#[test]
fn a_planted_index_cache_in_the_project_is_ignored() {
    let _env = clew_core::env_lock();
    let root = fixture_project("forged-cache");
    let data = root.parent().unwrap().join("forged-cache-data");
    let _ = std::fs::remove_dir_all(&data);
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::set_var("CLEW_DATA_DIR", &data) };
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

    let mut app = App::blank();
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
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::remove_var("CLEW_DATA_DIR") };
}
