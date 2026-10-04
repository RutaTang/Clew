use super::*;
use clew_protocol::{StateEdit, StateMerge};

fn target(value: Option<&str>) -> StateMerge {
    StateMerge {
        key_fields: vec!["target".into()],
        key: Vec::new(),
        edit: StateEdit::TomlString(value.map(str::to_string)),
        delete_when_empty: true,
    }
}

#[test]
fn a_toml_edit_preserves_current_unknown_keys_and_replays_without_overwriting() {
    let root = crate::testutil::TempDir::new("toml-current-keys");
    let path = root.join(".clew/reading.toml");
    // This key appeared after the client loaded its target. The operation
    // carries no snapshot that could overwrite it.
    write_atomic(&path, b"wrap = true\n[future]\ncolumns = 3\n").unwrap();
    let first = merge_file(&path, &target(Some("Windows (x86_64)")), "a-1").unwrap();
    assert!(first.applied);
    let text = first.text.unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(table["wrap"].as_bool(), Some(true));
    assert_eq!(table["future"]["columns"].as_integer(), Some(3));

    // Another writer changes the target and adds a key before a lost reply
    // is replayed. The replay must return the current file unchanged.
    let _ = merge_file(&path, &target(Some("Linux (x86_64)")), "b-1").unwrap();
    let mut table: toml::Table = toml::from_str(&read_checked(&path).unwrap().unwrap()).unwrap();
    table.insert("font_size".into(), 18.into());
    write_atomic(&path, toml::to_string(&table).unwrap().as_bytes()).unwrap();
    let before = read_checked(&path).unwrap();
    let replay = merge_file(&path, &target(Some("Windows (x86_64)")), "a-1").unwrap();
    assert!(!replay.applied);
    assert_eq!(replay.text, before);

    let host = merge_file(&path, &target(None), "a-2")
        .unwrap()
        .text
        .unwrap();
    let table: toml::Table = toml::from_str(&host).unwrap();
    assert!(!table.contains_key("target"));
    assert_eq!(table["font_size"].as_integer(), Some(18));
    assert_eq!(table["future"]["columns"].as_integer(), Some(3));
}

#[test]
fn removing_a_toml_key_deletes_only_a_file_with_no_other_keys() {
    let root = crate::testutil::TempDir::new("toml-empty");
    let path = root.join(".clew/reading.toml");
    merge_file(&path, &target(Some("Windows (x86_64)")), "a-1").unwrap();
    assert!(
        merge_file(&path, &target(None), "a-2")
            .unwrap()
            .text
            .is_none()
    );
    assert!(!path.exists());
}

#[test]
fn a_toml_replay_validates_the_current_file_without_applying_the_old_value() {
    let root = crate::testutil::TempDir::new("toml-replay-near-cap");
    let path = root.join(".clew/reading.toml");
    let op = target(Some("Windows (x86_64)"));
    merge_file(&path, &op, "a-1").unwrap();
    // Another writer removed the target and filled this file with its own
    // preference. Re-applying the old target would exceed the cap, but a
    // replay merely returns this valid current file.
    let current = format!(
        "padding = \"{}\"\n",
        "x".repeat(MAX_TOML_STATE_BYTES as usize - 13)
    );
    assert_eq!(current.len() as u64, MAX_TOML_STATE_BYTES);
    write_atomic(&path, current.as_bytes()).unwrap();
    let replay = merge_file(&path, &op, "a-1").unwrap();
    assert!(!replay.applied);
    assert_eq!(replay.text.as_deref(), Some(current.as_str()));
    assert_eq!(std::fs::read_to_string(path).unwrap(), current);
}

#[test]
fn toml_edits_refuse_corruption_newer_schema_and_oversize_even_for_deletion() {
    let root = crate::testutil::TempDir::new("toml-refusal");
    let path = root.join(".clew/reading.toml");
    for (i, text) in [
        "target = [\n".to_string(),
        "schema_version = 2\ntarget = \"Windows (x86_64)\"\n".to_string(),
        format!("#{}\n", "x".repeat(MAX_TOML_STATE_BYTES as usize)),
    ]
    .into_iter()
    .enumerate()
    {
        write_atomic(&path, text.as_bytes()).unwrap();
        for (j, value) in [None, Some("Linux (x86_64)")].into_iter().enumerate() {
            let error = merge_file(&path, &target(value), &format!("refused-{i}-{j}")).unwrap_err();
            assert!(is_refusal(&error), "{error}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        }
    }
}

#[test]
fn toml_edits_refuse_bad_key_shapes_and_mismatched_file_formats() {
    let root = crate::testutil::TempDir::new("toml-shape");
    let toml_path = root.join(".clew/reading.toml");
    write_atomic(&toml_path, b"wrap = true\n").unwrap();
    for (i, op) in [
        StateMerge {
            key_fields: Vec::new(),
            ..target(None)
        },
        StateMerge {
            key_fields: vec!["".into()],
            ..target(None)
        },
        StateMerge {
            key_fields: vec!["target".into(), "wrap".into()],
            ..target(None)
        },
        StateMerge {
            key: vec!["unexpected".into()],
            ..target(None)
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert!(is_refusal(
            &merge_file(&toml_path, &op, &format!("bad-{i}")).unwrap_err()
        ));
        assert_eq!(
            read_checked(&toml_path).unwrap().as_deref(),
            Some("wrap = true\n")
        );
    }
    let json_path = root.join(".clew/bookmarks.json");
    write_atomic(&json_path, b"[]").unwrap();
    assert!(is_refusal(
        &merge_file(&json_path, &target(None), "bad-json").unwrap_err()
    ));
    let array_edit = StateMerge {
        key_fields: vec!["rel".into()],
        key: vec!["a.rs".into()],
        edit: StateEdit::Remove,
        delete_when_empty: true,
    };
    assert!(is_refusal(
        &merge_file(&toml_path, &array_edit, "bad-toml").unwrap_err()
    ));
}

#[test]
fn successive_target_choices_keep_their_order_and_can_coalesce_before_sending() {
    let windows = target(Some("Windows (x86_64)"));
    let linux = target(Some("Linux (x86_64)"));
    let host = target(None);
    assert!(supersedes(&linux, &windows));
    assert!(supersedes(&host, &linux));
    assert!(!commutes(&windows, &linux));
    assert!(!commutes(&linux, &host));
    assert!(commutes(&host, &host));
}
