//! Ask retrieval vectors may score only an index of the same embedding space.

use super::*;

fn indexed_ask(tag: &str) -> (App, embed::Space, DataDirOverride) {
    let hold = embeddings_at(&format!("{tag}-data"), "http://127.0.0.1:1/v1");
    let mut app = scanned_app(tag);
    app.llm_config_cache = Some(Some(unreachable_llm()));
    let space = embed::stored_space();
    let node = explain::Node::File(app.proj.project.as_ref().unwrap().root.join("src/lib.rs"));
    app.proj.embed_index = embed::Index {
        model: space.model.clone(),
        base_url: space.base_url.clone(),
        entries: vec![embed::Entry {
            node: node.clone(),
            hash: 1,
            vec: vec![1.0, 0.0],
        }],
    };
    app.proj.explain.cache.insert(
        node,
        explain::Cached {
            summary: "The old-space point implementation.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    (app, space, hold)
}

fn begin_indexed_question(app: &mut App) -> u64 {
    // The returned HTTP task is not run: tests supply the endpoint's reply
    // through the production Retrieved message and inspect what may be sent.
    let _ = app.on_ask_submit_rag("what is Point?".into());
    assert!(app.proj.asking);
    app.proj.inflight.ask_retrieval.as_ref().unwrap().0
}

fn retrieve(app: &mut App, stream: u64, space: Option<embed::Space>) -> Task<Message> {
    app.update(Message::Ask(AskMsg::Retrieved {
        stamp: app.stamp(),
        stream,
        question: "what is Point?".into(),
        space,
        qvec: Ok(vec![1.0, 0.0]),
    }))
}

fn assert_retrieval_rejected(app: &App) {
    assert!(!app.proj.asking);
    assert!(!app.ask_stream_active());
    assert!(app.proj.inflight.ask_context.is_none());
    assert!(app.proj.ask_turns.is_empty());
    assert_eq!(app.proj.ask_input, "what is Point?");
    assert!(
        app.status
            .contains("embedding index or configuration changed"),
        "{}",
        app.status
    );
}

#[test]
fn an_old_query_rejection_preserves_the_readers_new_draft() {
    let (mut app, old_space, _hold) = indexed_ask("ask-space-new-draft");
    let stream = begin_indexed_question(&mut app);
    let mut cfg = embed::Config::current_or_default();
    cfg.model = "new-model".into();
    cfg.save().unwrap();
    app.proj.ask_input = "a new question".into();
    assert!(run_task(retrieve(&mut app, stream, Some(old_space))).is_empty());
    assert!(!app.proj.asking);
    assert!(!app.ask_stream_active());
    assert_eq!(app.proj.ask_input, "a new question");
}

#[test]
fn ask_rejects_an_old_query_when_same_sized_new_space_vectors_land() {
    for change_model in [true, false] {
        let (mut app, old_space, _hold) = indexed_ask("ask-space-new-index");
        let stream = begin_indexed_question(&mut app);
        let mut cfg = embed::Config::current_or_default();
        if change_model {
            cfg.model = "new-model".into();
        } else {
            cfg.base_url = "http://127.0.0.1:2/v1".into();
        }
        cfg.save().unwrap();
        let mut index = app.proj.embed_index.clone();
        index.model = cfg.model.clone();
        index.base_url = cfg.base_url.clone();
        let _ = app.update(Message::Semantic(SemanticMsg::IndexMerged {
            stamp: app.stamp(),
            index: Handoff::new(index.clone()),
            saved: Ok(()),
        }));
        let mut rx = attach_server(&mut app);
        assert!(run_task(retrieve(&mut app, stream, Some(old_space))).is_empty());
        assert_retrieval_rejected(&app);
        assert_eq!(app.proj.embed_index.space(), index.space());
        assert!(
            rx.try_recv().is_err(),
            "rejected grounding sent an AI request"
        );
    }
}

#[test]
fn ask_discards_foreign_held_vectors_when_the_query_returns() {
    let (mut app, old_space, _hold) = indexed_ask("ask-space-foreign-held");
    let stream = begin_indexed_question(&mut app);
    let mut cfg = embed::Config::current_or_default();
    cfg.model = "new-model".into();
    cfg.save().unwrap();
    assert!(run_task(retrieve(&mut app, stream, Some(old_space))).is_empty());
    assert_retrieval_rejected(&app);
    assert!(app.proj.embed_index.entries.is_empty());
}

#[test]
fn ask_reports_a_same_space_index_build_as_a_retry_instead_of_empty_context() {
    let (mut app, space, _hold) = indexed_ask("ask-space-during-build");
    let stream = begin_indexed_question(&mut app);
    let _ = app.update(Message::Semantic(SemanticMsg::BuildIndex));
    assert!(app.proj.building_embeddings);
    assert!(app.proj.embed_index.entries.is_empty());
    assert!(run_task(retrieve(&mut app, stream, Some(space))).is_empty());
    assert_retrieval_rejected(&app);
    assert!(app.status.contains("index is ready"));
}

#[test]
fn rotating_only_an_embedding_key_keeps_ask_retrieval_and_scores() {
    let (mut app, space, _hold) = indexed_ask("ask-space-key-rotation");
    let stream = begin_indexed_question(&mut app);
    let mut cfg = embed::Config::current_or_default();
    cfg.api_key = "replacement-key".into();
    cfg.save().unwrap();
    let context = retrieve(&mut app, stream, Some(space));
    let pending = app.proj.inflight.ask_context.as_ref().unwrap();
    assert_eq!(pending.sources.len(), 1);
    assert_eq!(pending.sources[0].1, 1.0);
    let messages = run_task(context);
    assert!(
        messages
            .iter()
            .any(|msg| matches!(msg, Message::Ask(AskMsg::ContextReady { .. })))
    );
}

#[test]
fn ask_context_keeps_its_validated_sources_if_a_new_index_lands_before_streaming() {
    let (mut app, space, _hold) = indexed_ask("ask-space-context-snapshot");
    let stream = begin_indexed_question(&mut app);
    let context = retrieve(&mut app, stream, Some(space));
    let sources = app
        .proj
        .inflight
        .ask_context
        .as_ref()
        .unwrap()
        .sources
        .clone();
    let mut cfg = embed::Config::current_or_default();
    cfg.model = "new-model".into();
    cfg.save().unwrap();
    let new_node = explain::Node::File(app.proj.project.as_ref().unwrap().root.join("notes.txt"));
    app.proj.embed_index = embed::Index {
        model: cfg.model,
        base_url: cfg.base_url,
        entries: vec![embed::Entry {
            node: new_node,
            hash: 2,
            vec: vec![1.0, 0.0],
        }],
    };
    let mut rx = attach_server(&mut app);
    for msg in run_task(context) {
        let _ = app.update(msg);
    }
    let request = rx.try_recv().unwrap();
    let clew_protocol::Request::ChatStream { messages, .. } = request.request else {
        panic!("the assembled question was not sent as a chat request");
    };
    assert!(
        messages
            .iter()
            .any(|msg| msg.content.contains("old-space point implementation"))
    );
    assert_eq!(app.proj.ask_turns.last().unwrap().sources, sources);
    assert_eq!(app.proj.ask_turns.last().unwrap().sources[0].1, 1.0);
}

#[test]
fn ask_without_vector_retrieval_keeps_live_grounding_across_space_changes() {
    for debugger in [false, true] {
        let (mut app, _, _hold) = indexed_ask("ask-space-live-grounding");
        app.proj.embed_index = embed::Index::default();
        let root = app.proj.project.as_ref().unwrap().root.clone();
        if debugger {
            app.debug.session = Some(DebugSession {
                client: None,
                status: DebugStatus::Stopped,
                thread_id: Some(1),
                frames: vec![dap::StackFrame {
                    id: 7,
                    name: "paused_origin".into(),
                    path: Some(root.join("src/lib.rs")),
                    line: 3,
                    column: 1,
                }],
                scopes: Vec::new(),
                watches: Vec::new(),
                output: Vec::new(),
                current: Some((root.join("src/lib.rs"), 3)),
                program: root.join("prog"),
                args: Vec::new(),
                cwd: root.clone(),
                addr: None,
            });
        } else {
            app.proj.ask_pins.push(ask_pin(&root));
        }
        let submitted = app.on_ask_submit_rag("what is Point?".into());
        let mut cfg = embed::Config::current_or_default();
        cfg.model = "new-model".into();
        cfg.save().unwrap();
        app.proj.embed_index = embed::Index {
            model: cfg.model,
            base_url: cfg.base_url,
            entries: vec![embed::Entry {
                node: explain::Node::File(root.join("notes.txt")),
                hash: 2,
                vec: vec![1.0, 0.0],
            }],
        };
        let mut context = Task::none();
        for msg in run_task(submitted) {
            assert!(matches!(
                &msg,
                Message::Ask(AskMsg::Retrieved { space: None, .. })
            ));
            context = app.update(msg);
        }
        assert!(
            app.proj
                .inflight
                .ask_context
                .as_ref()
                .unwrap()
                .sources
                .is_empty()
        );
        let mut rx = attach_server(&mut app);
        for msg in run_task(context) {
            let _ = app.update(msg);
        }
        let request = rx.try_recv().unwrap();
        let clew_protocol::Request::ChatStream { messages, .. } = request.request else {
            panic!("live grounding did not reach the chat request");
        };
        let context: String = messages.into_iter().map(|msg| msg.content).collect();
        assert!(context.contains(if debugger {
            "paused_origin"
        } else {
            "Selected code"
        }));
        assert!(app.proj.ask_turns.last().unwrap().sources.is_empty());
    }
}

#[test]
fn stopped_ask_results_do_not_validate_or_mutate_the_next_questions_index() {
    let (mut app, old_space, _hold) = indexed_ask("ask-space-retired-stream");
    let stream = begin_indexed_question(&mut app);
    let _ = app.update(Message::Ask(AskMsg::Stop));
    let mut cfg = embed::Config::current_or_default();
    cfg.model = "new-model".into();
    cfg.save().unwrap();
    // The held old index is foreign now. A retired result must not even enter
    // validation and drop it, or overwrite the Stop status / user's input.
    let index = app.proj.embed_index.clone();
    app.proj.ask_input = "a new question".into();
    let status = app.status.clone();
    assert!(run_task(retrieve(&mut app, stream, Some(old_space))).is_empty());
    assert_eq!(app.proj.embed_index.space(), index.space());
    assert_eq!(app.proj.embed_index.entries.len(), index.entries.len());
    assert_eq!(app.proj.embed_index.entries[0].node, index.entries[0].node);
    assert_eq!(app.proj.embed_index.entries[0].vec, index.entries[0].vec);
    assert_eq!(app.proj.ask_input, "a new question");
    assert_eq!(app.status, status);
}

#[test]
fn ask_captures_the_space_of_the_real_embedding_request_before_settings_change() {
    use std::io::Write;
    use std::time::Duration;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (arrived_tx, arrived) = std::sync::mpsc::channel();
    let (answer_tx, answer) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let request = clew_core::testutil::read_http_request(&mut conn);
        arrived_tx.send(request).unwrap();
        answer.recv_timeout(Duration::from_secs(10)).unwrap();
        let body = r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#;
        write!(conn, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let _hold = embeddings_at("ask-space-real-request-data", &base);
    let mut app = scanned_app("ask-space-real-request");
    app.llm_config_cache = Some(Some(unreachable_llm()));
    let old_space = embed::stored_space();
    app.proj.embed_index = embed::Index {
        model: old_space.model.clone(),
        base_url: old_space.base_url.clone(),
        entries: vec![embed::Entry {
            node: explain::Node::File(app.proj.project.as_ref().unwrap().root.join("src/lib.rs")),
            hash: 1,
            vec: vec![1.0, 0.0],
        }],
    };
    let submitted = app.on_ask_submit_rag("what is Point?".into());
    let pending = std::thread::spawn(move || run_task(submitted));
    let body = arrived.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(body.contains("\"model\":\"m\""));
    let _ = app.update(Message::Settings(SettingsMsg::Open));
    let _ = app.update(Message::Settings(SettingsMsg::EmbedModelChanged(
        "new-model".into(),
    )));
    let _ = app.update(Message::Settings(SettingsMsg::Saved));
    answer_tx.send(()).unwrap();
    let mut rx = attach_server(&mut app);
    for msg in pending.join().unwrap() {
        assert!(
            matches!(&msg, Message::Ask(AskMsg::Retrieved { space: Some(space), .. }) if *space == old_space)
        );
        assert!(run_task(app.update(msg)).is_empty());
    }
    peer.join().unwrap();
    assert_retrieval_rejected(&app);
    assert!(
        rx.try_recv().is_err(),
        "a stale-space result sent a paid AI request"
    );
}
