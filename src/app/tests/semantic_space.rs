//! Query embeddings must retain their space across settings and index changes.

use super::*;

fn query_in_space(tag: &str) -> (App, embed::Space, DataDirOverride) {
    let hold = embeddings_at(&format!("{tag}-data"), "http://127.0.0.1:1/v1");
    let mut app = scanned_app(tag);
    let space = embed::stored_space();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.embed_index = embed::Index {
        model: space.model.clone(),
        base_url: space.base_url.clone(),
        entries: vec![embed::Entry {
            node: explain::Node::File(root.join("src/lib.rs")),
            hash: 1,
            vec: vec![1.0, 0.0],
        }],
    };
    let _ = app.update(Message::Semantic(SemanticMsg::QueryChanged("query".into())));
    let _ = app.update(Message::Semantic(SemanticMsg::Search));
    assert!(app.proj.searching_semantic);
    (app, space, hold)
}

fn finish_query(app: &mut App, seq: u64, space: embed::Space) {
    let _ = app.update(Message::Semantic(SemanticMsg::Results {
        stamp: app.stamp(),
        seq,
        query: "query".into(),
        space,
        result: Ok(vec![1.0, 0.0]),
    }));
}

#[test]
fn a_settings_space_change_retires_a_query_before_the_new_index_lands() {
    let (mut app, old_space, _hold) = query_in_space("semantic-space-settings");
    let seq = app.proj.semantic_seq;
    let cancelled = app.proj.inflight.semantic_search.as_ref().unwrap().flag();
    let mut new_index = app.proj.embed_index.clone();
    new_index.model = "new-model".into();
    let _ = app.update(Message::Settings(SettingsMsg::Open));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedModelChanged(
        new_index.model.clone(),
    )));
    let _ = app.update(Message::Settings(SettingsMsg::Saved));
    assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!app.proj.searching_semantic);
    let _ = app.update(Message::Semantic(SemanticMsg::IndexMerged {
        stamp: app.stamp(),
        index: Handoff::new(new_index.clone()),
        saved: Ok(()),
    }));
    let status = app.status.clone();
    finish_query(&mut app, seq, old_space);
    assert!(app.proj.semantic_results.is_empty());
    assert_eq!(app.proj.embed_index.space(), new_index.space());
    assert_eq!(
        app.status, status,
        "an old reply must not replace build status"
    );
}

#[test]
fn another_windows_space_change_is_checked_when_the_query_returns() {
    for change_model in [true, false] {
        let (mut app, old_space, _hold) = query_in_space("semantic-space-other-window");
        let seq = app.proj.semantic_seq;
        let mut cfg = embed::Config::current_or_default();
        if change_model {
            cfg.model = "new-model".into();
        } else {
            cfg.base_url = "http://127.0.0.1:2/v1".into();
        }
        cfg.save().unwrap();
        // A new-space build finishes before the old query. Equal dimensions
        // make this a silent, high-confidence wrong match without the guard.
        let mut index = app.proj.embed_index.clone();
        index.model = cfg.model.clone();
        index.base_url = cfg.base_url.clone();
        let _ = app.update(Message::Semantic(SemanticMsg::IndexMerged {
            stamp: app.stamp(),
            index: Handoff::new(index),
            saved: Ok(()),
        }));
        finish_query(&mut app, seq, old_space);
        assert!(app.proj.semantic_results.is_empty());
        assert_eq!(app.proj.embed_index.space(), cfg.space());
        assert!(
            app.status.contains("configuration changed"),
            "{}",
            app.status
        );
    }
}

#[test]
fn a_late_query_discards_an_index_the_current_config_disowns() {
    let (mut app, space, _hold) = query_in_space("semantic-query-foreign-index");
    let seq = app.proj.semantic_seq;
    let mut cfg = embed::Config::current_or_default();
    cfg.model = "other-model".into();
    cfg.save().unwrap();
    finish_query(&mut app, seq, space);
    assert!(app.proj.embed_index.entries.is_empty());
    assert!(app.proj.semantic_results.is_empty());
}

#[test]
fn rotating_a_key_or_saving_unchanged_settings_keeps_same_space_queries() {
    for rotate_key in [false, true] {
        let (mut app, space, _hold) = query_in_space("semantic-query-same-space");
        let seq = app.proj.semantic_seq;
        let _ = app.update(Message::Settings(SettingsMsg::Open));
        if rotate_key {
            let _ = app.update(Message::Settings(SettingsMsg::EmbedKeyChanged(
                "replacement-key".into(),
            )));
        }
        let _ = app.update(Message::Settings(SettingsMsg::Saved));
        finish_query(&mut app, seq, space);
        assert_eq!(app.proj.semantic_results.len(), 1);
        assert_eq!(app.proj.semantic_results[0].1, 1.0);
    }
}

#[test]
fn building_an_index_retires_a_query_before_taking_its_vectors() {
    let (mut app, space, _hold) = query_in_space("semantic-query-during-build");
    let seq = app.proj.semantic_seq;
    let cancelled = app.proj.inflight.semantic_search.as_ref().unwrap().flag();
    let node = app.proj.embed_index.entries[0].node.clone();
    app.proj.explain.cache.insert(
        node,
        explain::Cached {
            summary: "Defines a point.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let _ = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    assert!(app.proj.building_embeddings);
    assert!(app.proj.embed_index.entries.is_empty());
    assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
    finish_query(&mut app, seq, space);
    assert_eq!(app.status, "Building semantic index…");
    assert!(!app.proj.searching_semantic);
}
