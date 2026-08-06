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
    let (sig, body, _) = gather_fn_detail_input(file, "parseAll", &HashMap::new()).expect("detail");
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
    let _ = app.update(Message::FileLoaded {
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

fn scanned_app(tag: &str) -> App {
    let root = fixture_project(tag);
    let mut app = App::blank();
    let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
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
    let _ = app.update(Message::TreeUpdated(fs_scan::scan(root.clone())));
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
    let _ = app.update(Message::TreeUpdated(stale));
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
    assert!(!clew_core::trust::Trust::load().is_root_trusted(&root));

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
    assert!(clew_core::trust::Trust::load().is_root_trusted(&root));

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
    let _ = app.update(Message::SearchDone { result });
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
    let _ = app.update(Message::FileLoaded {
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
    let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
    assert!(app.managed_languages().contains(&"go".to_string()));
    assert_eq!(
        lsp::registry::default_for_language("go").unwrap().name,
        "gopls"
    );
}

/// A custom `command` in `.clew/lsp.toml` bypasses the store and starts
/// directly — no download prompt.
#[test]
/// A `command` in the project's own lsp.toml must not run silently: the file
/// ships with the repository, so a hostile one could otherwise execute anything
/// as soon as a matching file is opened.
fn custom_command_requires_approval() {
    let root = fixture_project("lsp-escape");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"/nonexistent/rust-analyzer\"\n",
    )
    .unwrap();
    let mut app = App::blank();
    let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
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
            .contains("/nonexistent/rust-analyzer")
    );
    assert_eq!(pending.language, "rust");

    // Declining leaves it unstarted.
    let _ = app.update(Message::LspCommandDismissed);
    assert!(app.pending_lsp_command.is_none());
    assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Unsupported(_))));
}

/// A definition result jumps to the target line and records history.
#[test]
fn definition_result_jumps_and_records_history() {
    let mut app = scanned_app("lsp-jump");
    open_synchronously(&mut app, "notes.txt", None);
    let target = app.project.as_ref().unwrap().root.join("src/lib.rs");

    let _ = app.update(Message::DefinitionResult {
        result: Ok(vec![lsp::client::Target {
            path: target.clone(),
            line: 2, // 0-based → jump to line 3
            character: 7,
        }]),
    });
    // open_file kicked off an async load; feed the FileLoaded it awaits.
    let content = read_text_file(&target).unwrap();
    let _ = app.update(Message::FileLoaded {
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
    let _ = app.update(Message::ScanDone(fs_scan::scan(root.clone())));
    open_synchronously(&mut app, "src/main.rs", None);

    // Start the real server (the escape hatch resolved it) and register it.
    let server = app.lsp_config.resolve("rust").unwrap();
    let client = lsp::client::LspClient::start(&server.command.unwrap(), &[], &root, None)
        .await
        .unwrap();
    let _ = app.update(Message::LspStartResult {
        language: "rust".into(),
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
        result: Ok(targets.clone()),
    });
    let content = read_text_file(&targets[0].path).unwrap();
    let _ = app.update(Message::FileLoaded {
        pane: 0,
        abs: targets[0].path.clone(),
        target: Some(targets[0].line + 1),
        result: Ok(content),
    });
    // Jumped to the `origin` definition on line 1 (1-based).
    assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
    assert!(app.history.can_back());
}
