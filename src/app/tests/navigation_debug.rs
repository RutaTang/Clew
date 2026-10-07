//! Search cancellation and DAP control-response ownership regressions.

use super::*;

fn assert_search_cleared(app: &App) {
    assert!(!app.proj.search.running);
    assert!(!app.proj.search.ran);
    assert!(app.proj.search.hits.is_empty());
    assert!(app.proj.search.error.is_none());
    assert!(app.proj.search.skipped.is_empty());
    assert!(app.proj.link.pending_search.is_none());
}

#[test]
fn an_empty_search_retires_the_running_fallback_and_clears_its_state() {
    let mut app = scanned_app("search-empty-fallback");
    let _ = app.update(Message::Nav(NavMsg::SearchQueryChanged("needle".into())));
    let old = app.update(Message::Nav(NavMsg::SearchSubmitted));
    let seq = app.proj.search_seq;
    assert!(app.proj.search.running);
    app.proj.search.error = Some("previous error".into());
    app.proj.search.skipped.push(search::SkippedFile {
        rel: "large.txt".into(),
        reason: "too large".into(),
    });
    let _ = app.update(Message::Nav(NavMsg::SearchQueryChanged(" \t ".into())));
    let _ = app.update(Message::Nav(NavMsg::SearchSubmitted));
    assert!(app.proj.search_seq > seq);
    assert_search_cleared(&app);
    let status = app.status.clone();
    for msg in run_task(old) {
        let _ = app.update(msg);
    }
    assert_search_cleared(&app);
    assert_eq!(app.status, status);
}

#[test]
fn an_empty_search_retires_server_results_errors_and_backoff() {
    for reply_error in [false, true] {
        let mut app = scanned_app("search-empty-server");
        let mut rx = attach_server(&mut app);
        let _ = app.update(Message::Nav(NavMsg::SearchQueryChanged("needle".into())));
        let _ = app.update(Message::Nav(NavMsg::SearchSubmitted));
        let request = rx.try_recv().unwrap();
        assert!(matches!(
            request.request,
            clew_protocol::Request::Search { .. }
        ));
        let _ = app.update(Message::Nav(NavMsg::SearchQueryChanged(String::new())));
        // Option toggles also submit an empty search.
        let _ = app.update(Message::Nav(NavMsg::SearchToggle(SearchOpt::Case)));
        assert_search_cleared(&app);
        let status = app.status.clone();
        let _ = app.on_resend_not_ready(request.id, request.request, 1);
        assert!(rx.try_recv().is_err());
        let event = if reply_error {
            clew_protocol::Event::Error {
                code: clew_protocol::ErrorCode::NotReady,
                message: "old search is not ready".into(),
            }
        } else {
            clew_protocol::Event::SearchResults {
                hits: vec![clew_protocol::SearchHit {
                    rel: "notes.txt".into(),
                    line: 1,
                    preview: "needle".into(),
                }],
                error: None,
                skipped: Vec::new(),
                skipped_total: 0,
            }
        };
        let late = app.handle_server_reply(request.id, event);
        assert!(run_task(late).is_empty());
        assert_search_cleared(&app);
        assert_eq!(app.status, status);
    }
}

#[test]
fn a_new_fallback_search_also_retires_an_older_server_request() {
    let mut app = scanned_app("search-server-to-fallback");
    let mut rx = attach_server(&mut app);
    let _ = app.update(Message::Nav(NavMsg::SearchQueryChanged("needle".into())));
    let _ = app.update(Message::Nav(NavMsg::SearchSubmitted));
    let old = rx.try_recv().unwrap();
    rx.close();
    let _ = app.update(Message::Nav(NavMsg::SearchQueryChanged("Point".into())));
    let new = app.update(Message::Nav(NavMsg::SearchSubmitted));
    assert!(app.proj.link.pending_search.is_none());
    let _ = app.handle_server_reply(
        old.id,
        clew_protocol::Event::SearchResults {
            hits: vec![clew_protocol::SearchHit {
                rel: "notes.txt".into(),
                line: 1,
                preview: "needle".into(),
            }],
            error: None,
            skipped: Vec::new(),
            skipped_total: 0,
        },
    );
    assert!(app.proj.search.hits.is_empty());
    assert!(app.proj.search.running);
    for msg in run_task(new) {
        let _ = app.update(msg);
    }
    assert!(!app.proj.search.running);
    assert!(app.proj.search.ran);
    assert!(!app.proj.search.hits.is_empty());
    assert!(
        app.proj
            .search
            .hits
            .iter()
            .all(|hit| hit.rel == "src/lib.rs")
    );
}

fn control_session(
    app: &mut App,
    reject: bool,
) -> (
    tokio::runtime::Runtime,
    crate::dap::client::DapEvents,
    DapSeen,
) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let seen = DapSeen::default();
    let rec = seen.clone();
    let (client, events) = rt.block_on(async move {
        let (to_adapter, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_adapter) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let answer = move |command: &str| {
                if reject && matches!(command, "continue" | "next" | "stepIn" | "stepOut") {
                    Err("Cannot leave the initial frame".to_string())
                } else {
                    Ok(serde_json::json!({}))
                }
            };
            serve_dap(from_client, to_client, rec, answer, "").await;
        });
        crate::dap::DapClient::connect(to_adapter, from_adapter)
            .await
            .unwrap()
    });
    let current = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    app.debug.session = Some(DebugSession {
        client: Some(client),
        status: DebugStatus::Stopped,
        thread_id: Some(1),
        frames: vec![dap::StackFrame {
            id: 7,
            name: "origin".into(),
            path: Some(current.clone()),
            line: 3,
            column: 1,
        }],
        scopes: vec![DebugScope {
            name: "locals".into(),
            vars: Vec::new(),
        }],
        watches: vec![("x".into(), "1".into())],
        output: Vec::new(),
        current: Some((current, 3)),
        program: PathBuf::from("/p/prog"),
        args: Vec::new(),
        cwd: PathBuf::from("/p"),
        addr: None,
    });
    (rt, events, seen)
}

fn deliver_dap_event(app: &mut App, event: dap::DapEvent) {
    let _ = app.update(Message::Debug(DebugMsg::DapEvent {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        event,
    }));
}

fn deliver_new_stop(app: &mut App) {
    deliver_dap_event(
        app,
        dap::DapEvent::Stopped(dap::proto::Stopped {
            reason: "step".into(),
            thread_id: Some(1),
            description: None,
            text: None,
            all_threads: true,
        }),
    );
    let _ = app.update(Message::Debug(DebugMsg::DapStopInspected {
        stamp: app.transport_stamp(),
        run: app.debug_run,
        stop: app.debug_stop,
        frames: vec![dap::StackFrame {
            id: 8,
            name: "new_stop".into(),
            path: None,
            line: 8,
            column: 1,
        }],
        scopes: Vec::new(),
    }));
}

#[test]
fn rejected_dap_controls_restore_the_pause_and_report_the_adapter_error() {
    let mut app = scanned_app("dap-control-rejected");
    let (_rt, _events, seen) = control_session(&mut app, true);
    let current = app.debug.session.as_ref().unwrap().current.clone();
    for cmd in [
        DebugCmd::Continue,
        DebugCmd::StepOver,
        DebugCmd::StepIn,
        DebugCmd::StepOut,
    ] {
        let old_stop = app.debug_stop;
        let task = app.update(Message::Debug(DebugMsg::Control(cmd)));
        assert_eq!(
            app.debug.session.as_ref().unwrap().status,
            DebugStatus::Running
        );
        assert!(app.debug.session.as_ref().unwrap().current.is_none());
        for msg in run_task(task) {
            let _ = app.update(msg);
        }
        let session = app.debug.session.as_ref().unwrap();
        assert_eq!(session.status, DebugStatus::Stopped);
        assert_eq!(session.current, current);
        assert_eq!(session.frames[0].id, 7);
        assert_eq!(session.scopes[0].name, "locals");
        assert_eq!(session.watches, vec![("x".into(), "1".into())]);
        assert!(app.status.contains("Cannot leave the initial frame"));
        // The failed resume must not re-authorize an old in-flight inspection.
        let _ = app.update(Message::Debug(DebugMsg::DapStopInspected {
            stamp: app.transport_stamp(),
            run: app.debug_run,
            stop: old_stop,
            frames: Vec::new(),
            scopes: Vec::new(),
        }));
        assert_eq!(app.debug.session.as_ref().unwrap().frames[0].id, 7);
        assert!(app.debug.trace.is_empty());
    }
    let commands: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(cmd, _)| cmd.clone())
        .collect();
    assert_eq!(commands, ["continue", "next", "stepIn", "stepOut"]);
}

#[test]
fn successful_dap_controls_ignore_duplicate_input_and_preserve_newer_events() {
    for events_first in [false, true] {
        let mut app = scanned_app("dap-control-success");
        let (_rt, _events, seen) = control_session(&mut app, false);
        let task = app.update(Message::Debug(DebugMsg::Control(DebugCmd::StepOut)));
        let pending_stop = app.debug_stop;
        let duplicate = app.update(Message::Debug(DebugMsg::Control(DebugCmd::Continue)));
        assert!(run_task(duplicate).is_empty());
        assert_eq!(app.debug_stop, pending_stop);
        let reply = run_task(task);
        if !events_first {
            for msg in reply.clone() {
                let _ = app.update(msg);
            }
            assert_eq!(
                app.debug.session.as_ref().unwrap().status,
                DebugStatus::Running
            );
            assert!(app.debug.session.as_ref().unwrap().current.is_none());
        }
        deliver_dap_event(
            &mut app,
            dap::DapEvent::Continued {
                thread_id: Some(1),
                all_threads: true,
            },
        );
        deliver_new_stop(&mut app);
        if events_first {
            for msg in reply {
                let _ = app.update(msg);
            }
        }
        assert_eq!(
            app.debug.session.as_ref().unwrap().status,
            DebugStatus::Stopped
        );
        assert_eq!(app.debug.session.as_ref().unwrap().frames[0].id, 8);
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|(cmd, _)| cmd == "stepOut")
                .count(),
            1
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}

#[test]
fn late_dap_control_errors_cannot_replace_newer_execution_or_ownership() {
    for later in [
        "continued",
        "stopped",
        "terminated",
        "run",
        "project",
        "transport",
        "child",
    ] {
        let mut app = scanned_app("dap-control-stale");
        let (_rt, _events, _) = control_session(&mut app, true);
        let task = app.update(Message::Debug(DebugMsg::Control(DebugCmd::StepOut)));
        let errors = run_task(task);
        assert!(
            errors
                .iter()
                .any(|msg| matches!(msg, Message::Debug(DebugMsg::DapControlFailed { .. })))
        );
        match later {
            "continued" => deliver_dap_event(
                &mut app,
                dap::DapEvent::Continued {
                    thread_id: Some(1),
                    all_threads: true,
                },
            ),
            "stopped" => deliver_new_stop(&mut app),
            "terminated" => deliver_dap_event(&mut app, dap::DapEvent::Terminated),
            "run" => app.bump_debug_run(),
            "project" => scan_synchronously(&mut app, fixture_project("dap-control-new-project")),
            "transport" => app.conn_gen += 1,
            "child" => {
                let client = app.debug.session.as_ref().unwrap().client.clone().unwrap();
                let _ = app.update(Message::Debug(DebugMsg::DapChildStarted {
                    stamp: app.transport_stamp(),
                    run: app.debug_run,
                    client,
                    hover_safe: false,
                }));
            }
            _ => unreachable!(),
        }
        let expected = app
            .debug
            .session
            .as_ref()
            .map(|session| (session.status, session.current.clone()));
        app.status = format!("newer {later}");
        for msg in errors {
            let _ = app.update(msg);
        }
        assert_eq!(
            app.debug
                .session
                .as_ref()
                .map(|session| (session.status, session.current.clone())),
            expected,
            "{later}"
        );
        assert_eq!(app.status, format!("newer {later}"));
    }
}

#[test]
fn a_retried_control_cannot_be_rolled_back_by_the_previous_error() {
    let mut app = scanned_app("dap-control-retry");
    let (_rt, _events, _) = control_session(&mut app, true);
    let first = app.update(Message::Debug(DebugMsg::Control(DebugCmd::StepOut)));
    let old_errors = run_task(first);
    for msg in old_errors.clone() {
        let _ = app.update(msg);
    }
    assert_eq!(
        app.debug.session.as_ref().unwrap().status,
        DebugStatus::Stopped
    );
    let retry = app.update(Message::Debug(DebugMsg::Control(DebugCmd::StepOver)));
    app.status = "retry pending".into();
    for msg in old_errors {
        let _ = app.update(msg);
    }
    assert_eq!(
        app.debug.session.as_ref().unwrap().status,
        DebugStatus::Running
    );
    assert_eq!(app.status, "retry pending");
    for msg in run_task(retry) {
        let _ = app.update(msg);
    }
    assert_eq!(
        app.debug.session.as_ref().unwrap().status,
        DebugStatus::Stopped
    );
    assert!(app.debug.session.as_ref().unwrap().current.is_some());
}
