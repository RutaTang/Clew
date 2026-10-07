//! Remote state adopts absence and writes reading preferences as key edits.

use super::*;

fn state_read(app: &mut App, root: &Path, rel: &str, text: Option<String>) {
    let _ = app.handle_server_event(clew_protocol::Event::StateContent {
        root: root.to_string_lossy().into_owned(),
        rel: rel.into(),
        text,
    });
}

#[test]
fn a_missing_remote_store_clears_its_clean_reconnected_copy() {
    let (mut app, root, _rx) = remote_app("remote-missing-clean");
    state_read(
        &mut app,
        &root,
        bookmarks::REL,
        Some(THREE_BOOKMARKS.into()),
    );
    state_read(
        &mut app,
        &root,
        notes::REL,
        Some(
            r#"[{"rel":"src/lib.rs","symbol":"origin","text":"a note","understood":true}]"#.into(),
        ),
    );
    state_read(
        &mut app,
        &root,
        "reading.toml",
        Some("target = \"Windows (x86_64)\"\n".into()),
    );
    app.proj.history.push(
        Loc {
            path: root.join("src/lib.rs"),
            line: Some(1),
        },
        None,
    );
    app.proj.trail_collapsed.insert(0);
    state_read(
        &mut app,
        &root,
        walkthrough::LIBRARY_REL,
        Some(format!("[{}]", tour_json("mine"))),
    );
    app.proj.walk.open = Some("mine".into());
    let (prepared, _) = app.prepare_segments("mine narration");
    app.proj.walk.prepared = prepared;
    assert!(!app.proj.bookmarks.is_empty());
    assert!(!app.proj.notes.is_empty());
    assert!(app.proj.history.loc(0).is_some());
    assert!(app.proj.walk.open.is_some());

    // Reconnect retains the displayed snapshots. Another client removed
    // each final entry (or restored the host target), so the host replies
    // with missing files rather than serialized empty lists.
    app.drop_connection_state();
    let _rx = attach_server(&mut app);
    app.request_remote_state();
    for rel in crate::app::remote_state::REMOTE_STATE_FILES {
        state_read(&mut app, &root, rel, None);
    }
    assert!(app.proj.bookmarks.is_empty());
    assert!(app.proj.notes.is_empty());
    assert!(app.proj.history.loc(0).is_none());
    assert!(app.proj.trail_collapsed.is_empty());
    assert_eq!(app.proj.reading_target, inactive::Target::host());
    assert!(app.proj.walk.library.is_empty());
    assert!(app.proj.walk.open.is_none());
    assert!(app.proj.walk.prepared.is_empty());
    assert!(app.proj.remote_state_pending.is_empty());
}

#[test]
fn a_reading_file_with_other_keys_and_no_target_restores_the_host_default() {
    let (mut app, root, _rx) = remote_app("remote-target-key-absent");
    state_read(
        &mut app,
        &root,
        "reading.toml",
        Some("target = \"Windows (x86_64)\"\n".into()),
    );
    state_read(
        &mut app,
        &root,
        "reading.toml",
        Some("wrap = true\n[future]\ncolumns = 3\n".into()),
    );
    assert_eq!(app.proj.reading_target, inactive::Target::host());
    // Corruption is not evidence that the saved target was removed.
    app.proj.reading_target = inactive::Target::from_label("Windows (x86_64)");
    state_read(&mut app, &root, "reading.toml", Some("target = [\n".into()));
    assert_eq!(
        app.proj.reading_target,
        inactive::Target::from_label("Windows (x86_64)")
    );
}

#[test]
fn remote_target_choices_preserve_keys_added_since_the_client_loaded() {
    let (mut app, root, mut rx) = remote_app("remote-target-merge");
    state_read(
        &mut app,
        &root,
        "reading.toml",
        Some("wrap = true\n".into()),
    );
    let path = root.join(".clew/reading.toml");
    clew_core::statefile::write_atomic(&path, b"wrap = true\n[future]\ncolumns = 3\n").unwrap();
    let windows = inactive::Target::from_label("Windows (x86_64)");
    let _ = app.on_target_selected(windows.clone());
    let (edits, writes) = took_state_edits(&mut rx, "reading.toml");
    assert!(
        writes.is_empty(),
        "target choices must never replace a snapshot"
    );
    let [(request, edit_id, merge)] = edits.as_slice() else {
        panic!("{edits:?}")
    };
    let merged = clew_core::statefile::merge_file(&path, merge, edit_id).unwrap();
    let table: toml::Table = toml::from_str(merged.text.as_deref().unwrap()).unwrap();
    assert_eq!(table["wrap"].as_bool(), Some(true));
    assert_eq!(table["future"]["columns"].as_integer(), Some(3));
    let _ = app.handle_server_reply(
        *request,
        clew_protocol::Event::StateEdited {
            root: root.to_string_lossy().into_owned(),
            rel: "reading.toml".into(),
            text: merged.text,
        },
    );
    assert_eq!(app.proj.reading_target, windows);
    assert!(!app.proj.remote_state_dirty.contains("reading.toml"));

    let _ = app.on_target_selected(inactive::Target::host());
    let (edits, writes) = took_state_edits(&mut rx, "reading.toml");
    assert!(writes.is_empty());
    let [(request, edit_id, merge)] = edits.as_slice() else {
        panic!("{edits:?}")
    };
    let merged = clew_core::statefile::merge_file(&path, merge, edit_id).unwrap();
    let table: toml::Table = toml::from_str(merged.text.as_deref().unwrap()).unwrap();
    assert!(!table.contains_key("target"));
    assert_eq!(table["future"]["columns"].as_integer(), Some(3));
    let _ = app.handle_server_reply(
        *request,
        clew_protocol::Event::StateEdited {
            root: root.to_string_lossy().into_owned(),
            rel: "reading.toml".into(),
            text: merged.text,
        },
    );
    assert_eq!(app.proj.reading_target, inactive::Target::host());
}

#[test]
fn a_missing_state_read_keeps_dirty_history_and_journaled_reading_changes() {
    let (mut app, root, mut rx) = remote_app("remote-missing-dirty");
    app.proj.history.push(
        Loc {
            path: root.join("src/lib.rs"),
            line: Some(1),
        },
        None,
    );
    app.proj.remote_state_dirty.insert("history.json".into());
    state_read(&mut app, &root, "history.json", None);
    assert!(app.proj.history.loc(0).is_some());
    assert!(app.proj.remote_state_dirty.contains("history.json"));

    let windows = inactive::Target::from_label("Windows (x86_64)");
    let linux = inactive::Target::from_label("Linux (x86_64)");
    let _ = app.on_target_selected(windows.clone());
    let (first, _) = took_state_edits(&mut rx, "reading.toml");
    let first_id = first[0].1.clone();
    app.drop_connection_state();
    let _ = app.on_target_selected(linux.clone());
    let mut rx = attach_server(&mut app);
    app.request_remote_state();
    state_read(&mut app, &root, "reading.toml", None);
    assert_eq!(app.proj.reading_target, linux);
    let (replayed, writes) = took_state_edits(&mut rx, "reading.toml");
    assert!(writes.is_empty());
    let [(request, id, _)] = replayed.as_slice() else {
        panic!("{replayed:?}")
    };
    assert_eq!(id, &first_id);
    let _ = app.handle_server_reply(
        *request,
        clew_protocol::Event::StateEdited {
            root: root.to_string_lossy().into_owned(),
            rel: "reading.toml".into(),
            text: Some("target = \"Windows (x86_64)\"\n".into()),
        },
    );
    assert_eq!(
        app.proj.reading_target, linux,
        "an older answer must not roll back the newer pick"
    );
    let (last, _) = took_state_edits(&mut rx, "reading.toml");
    let [(request, _, _)] = last.as_slice() else {
        panic!("{last:?}")
    };
    let _ = app.handle_server_reply(
        *request,
        clew_protocol::Event::StateEdited {
            root: root.to_string_lossy().into_owned(),
            rel: "reading.toml".into(),
            text: Some("target = \"Linux (x86_64)\"\n".into()),
        },
    );
    assert_eq!(app.proj.reading_target, linux);
    assert!(!app.proj.remote_state_dirty.contains("reading.toml"));
}

#[test]
fn a_host_target_merge_that_deletes_the_file_adopts_the_default() {
    let (mut app, root, mut rx) = remote_app("remote-host-target-empty");
    state_read(
        &mut app,
        &root,
        "reading.toml",
        Some("target = \"Windows (x86_64)\"\n".into()),
    );
    let _ = app.on_target_selected(inactive::Target::host());
    let (edits, _) = took_state_edits(&mut rx, "reading.toml");
    let [(request, _, _)] = edits.as_slice() else {
        panic!("{edits:?}")
    };
    let _ = app.handle_server_reply(
        *request,
        clew_protocol::Event::StateEdited {
            root: root.to_string_lossy().into_owned(),
            rel: "reading.toml".into(),
            text: None,
        },
    );
    assert_eq!(app.proj.reading_target, inactive::Target::host());
    assert!(!app.proj.remote_state_dirty.contains("reading.toml"));
    assert!(app.proj.remote_edits.is_empty());
}

#[test]
fn a_missing_authoritative_read_undoes_an_unsaved_first_bookmark() {
    let (mut app, root, mut rx) = remote_app("remote-first-bookmark-refused");
    assert!(app.edit_remote_state(
        bookmarks::REL,
        bookmarks::merge_toggle("src/lib.rs", 1, "first".into())
    ));
    bookmarks::toggle(&mut app.proj.bookmarks, "src/lib.rs", 1, "first".into());
    let (edits, _) = took_state_edits(&mut rx, bookmarks::REL);
    let [(request, _, _)] = edits.as_slice() else {
        panic!("{edits:?}")
    };
    // A permanent refusal ends the journal entry and asks for authoritative
    // state, to undo the optimistic copy rather than showing it as saved.
    let _ = app.handle_server_reply(
        *request,
        clew_protocol::Event::Error {
            code: clew_protocol::ErrorCode::Refused,
            message: "not saved".into(),
        },
    );
    assert!(app.proj.remote_edits.is_empty());
    state_read(&mut app, &root, bookmarks::REL, None);
    assert!(app.proj.bookmarks.is_empty());
}

#[test]
fn a_large_local_notebook_save_reaches_the_cell_refresh_request() {
    let mut app = scanned_app("large-notebook-watch");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let mut rx = attach_server(&mut app);
    let path = root.join("analysis.IPYNB");
    let raw = |value: usize| {
        serde_json::json!({
            "nbformat": 4, "nbformat_minor": 5,
            "metadata": { "padding": "x".repeat(viewer::MAX_FILE_BYTES + 1024) },
            "cells": [{ "cell_type": "code", "metadata": {}, "execution_count": null,
                "outputs": [], "source": [format!("value = {value}\n")] }],
        })
        .to_string()
    };
    let first = raw(1);
    assert!(clew_core::notebook::parse(&first).is_some());
    assert!(first.len() > viewer::MAX_FILE_BYTES);
    std::fs::write(&path, &first).unwrap();
    let code_cell = |value| clew_protocol::NotebookCell {
        kind: "code".into(),
        source: format!("value = {value}\n"),
        lines: Vec::new(),
        proj_line: 2,
        outputs: Vec::new(),
        execution_count: None,
    };
    let _ = app.apply_notebook_content(
        &[0],
        None,
        "analysis.IPYNB".into(),
        "python".into(),
        vec![code_cell(1)],
        Vec::new(),
        "# %%\nvalue = 1\n".into(),
        false,
    );
    while rx.try_recv().is_ok() {}
    std::fs::write(&path, raw(2)).unwrap();
    let task = app.on_files_changed(vec![path]);
    drive(&mut app, task, |m| {
        matches!(m, Message::Watch(WatchMsg::FilesRehashed { .. }))
    });
    let (request, _) = read_request(&mut rx, "analysis.IPYNB");
    let _ = app.handle_server_reply(
        request,
        clew_protocol::Event::NotebookContent {
            rel: "analysis.IPYNB".into(),
            language: "python".into(),
            cells: vec![code_cell(2)],
            symbols: Vec::new(),
            projection: "# %%\nvalue = 2\n".into(),
        },
    );
    assert_eq!(
        app.proj.panes[0]
            .as_ref()
            .unwrap()
            .notebook
            .as_ref()
            .unwrap()
            .cells[0]
            .source,
        "value = 2\n"
    );
    let text_path = root.join("large.txt");
    std::fs::write(&text_path, first).unwrap();
    assert!(
        watch::rehash(&root, vec![(text_path, 0)], viewer::MAX_FILE_BYTES as u64).is_empty(),
        "ordinary source/text files keep the original cap"
    );
}
