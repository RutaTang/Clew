//! The setup sheet (`crate::app::setup`): a first open asks once — where
//! clew keeps its data, its language servers, explaining it — and its
//! answer is carried out: the leave to build recorded, servers fetched once
//! the project is open, nothing paid for unless ticked.

use super::*;
use crate::app::setup::{Census, Fetch, SetupMode, SetupServer};

fn builds_and_downloads() -> SetupServer {
    SetupServer {
        language: "rust".into(),
        name: "rust-analyzer".into(),
        fetch: Fetch::Download,
        builds: true,
    }
}

/// An untrusted folder opens the sheet, not the project; its look at the
/// project lands in it; Not now opens nothing and records nothing.
#[test]
fn a_first_open_asks_in_the_sheet_and_not_now_opens_nothing() {
    let data = test_dir("setup-data-deny");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-deny");
    let mut app = blank_app();
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        root.clone(),
    ))));
    assert_eq!(app.pending_consent.as_deref(), Some(root.as_path()));
    let setup = app.setup.as_ref().expect("the sheet");
    assert_eq!(setup.mode, SetupMode::Open);
    assert!(setup.servers, "servers ticked");
    assert!(!setup.explain, "explaining costs money: not ticked");
    assert!(setup.census.is_none(), "still looking");

    let census = Census {
        servers: vec![builds_and_downloads()],
        calls: Some(42),
    };
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root: root.clone(),
        census: census.clone(),
    }));
    assert_eq!(app.setup.as_ref().unwrap().census.as_ref(), Some(&census));

    let _ = app.update(Message::Project(ProjectMsg::ConsentDenied(root.clone())));
    assert!(app.setup.is_none() && app.pending_consent.is_none());
    assert!(app.proj.project.is_none() && !app.scanning);
    assert!(app.pending_setup.is_none());
    let trust = clew_core::trust::Trust::load();
    assert!(!trust.is_root_trusted(None, &root));
    assert!(!trust.is_build_trusted(None, &root, "rust-analyzer"));
}

/// Open with the servers ticked: the leave to build is recorded at once —
/// no "Let the language server build this project?" later — and the
/// downloads wait for the project to be open. Explaining, not ticked,
/// starts nothing.
#[test]
fn opening_records_the_leave_to_build_and_queues_the_downloads() {
    let data = test_dir("setup-data-open");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-open");
    let mut app = blank_app();
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        root.clone(),
    ))));
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root: root.clone(),
        census: Census {
            servers: vec![builds_and_downloads()],
            calls: Some(42),
        },
    }));
    let _ = app.update(Message::Project(ProjectMsg::ConsentAllowed(root.clone())));
    assert!(app.setup.is_none(), "answered");
    let trust = clew_core::trust::Trust::load();
    assert!(trust.is_root_trusted(None, &root));
    assert!(trust.is_build_trusted(None, &root, "rust-analyzer"));
    let intent = app.pending_setup.as_ref().expect("the downloads wait");
    assert_eq!(intent.fetch, vec!["rust".to_string()]);
    assert!(!intent.explain);
}

/// Servers unticked: nothing recorded, nothing queued — they ask as ever,
/// when a file of their language opens.
#[test]
fn servers_unticked_leave_the_questions_for_later() {
    let data = test_dir("setup-data-unticked");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-unticked");
    let mut app = blank_app();
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        root.clone(),
    ))));
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root: root.clone(),
        census: Census {
            servers: vec![builds_and_downloads()],
            calls: Some(42),
        },
    }));
    let _ = app.update(Message::Project(ProjectMsg::SetupServers(false)));
    let _ = app.update(Message::Project(ProjectMsg::ConsentAllowed(root.clone())));
    assert!(!clew_core::trust::Trust::load().is_build_trusted(None, &root, "rust-analyzer"));
    assert!(app.pending_setup.is_none());
}

/// Without an AI key, explaining cannot be ticked; a look that found
/// nothing to set up for the servers leaves them unticked.
#[test]
fn what_cannot_be_done_cannot_be_ticked() {
    let mut app = scanned_app("setup-untickable");
    app.llm_available = false;
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let _ = app.show_setup(SetupMode::Manage, root.clone());
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root,
        census: Census {
            servers: Vec::new(),
            calls: Some(10),
        },
    }));
    let _ = app.update(Message::Project(ProjectMsg::SetupExplain(true)));
    let setup = app.setup.as_ref().unwrap();
    assert!(!setup.explain, "no key, no explaining");
    assert!(!setup.servers, "nothing to set up for them");
    // Cancel puts it away; nothing was done.
    let out = run_task(app.update(Message::Project(ProjectMsg::SetupClose)));
    assert!(app.setup.is_none() && out.is_empty());
}

/// A first open's sheet is answered with Open or Not now — Escape and the
/// backdrop decline it, and SetupClose does not drop it unanswered.
#[test]
fn a_first_opens_sheet_waits_for_its_answer() {
    let data = test_dir("setup-data-wait");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-wait");
    let mut app = blank_app();
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        root.clone(),
    ))));
    let _ = app.update(Message::Project(ProjectMsg::SetupClose));
    assert!(app.setup.is_some() && app.pending_consent.is_some());
    assert!(matches!(
        crate::ui::escape_message(&app),
        Some(Message::Project(ProjectMsg::ConsentDenied(r))) if r == root
    ));
}

/// The look at the project counts what explaining it costs, less what is
/// explained already.
#[test]
fn the_count_leaves_out_what_is_explained() {
    let mut app = scanned_app("setup-explained");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let _ = app.show_setup(SetupMode::Manage, root.clone());
    app.explain_cache_mut().insert(
        explain::Node::File(root.join("src/lib.rs")),
        explain::Cached {
            summary: "s".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root,
        census: Census {
            servers: Vec::new(),
            calls: Some(10),
        },
    }));
    let calls = app.setup.as_ref().unwrap().census.as_ref().unwrap().calls;
    assert_eq!(calls, Some(9));
}

/// The sheet asking about the open project — "Set Up Project…", Explain
/// All asking first — goes with it when another project opens under it (a
/// scan already on its way, ⌘O from the menu bar): left up, still naming
/// the first project and its cost, its Explain started a paid pass over the
/// second.
#[test]
fn a_sheet_about_the_open_project_goes_with_it() {
    let mut app = scanned_app("setup-left-a");
    let first = app.proj.project.as_ref().unwrap().root.clone();
    let _ = app.show_setup(SetupMode::Explain, first.clone());
    assert!(app.setup.is_some());
    let second = fixture_project("setup-left-b");
    scan_synchronously(&mut app, second.clone());
    assert_eq!(
        app.proj.project.as_ref().map(|p| p.root.clone()),
        Some(second)
    );
    assert!(app.setup.is_none(), "the sheet about {first:?} stayed up");
    let out = run_task(app.update(Message::Project(ProjectMsg::SetupApply)));
    assert!(out.is_empty() && !app.proj.explain.running);
}

/// A first open's sheet holds the one sheet there is until it is answered:
/// Explain All (its chord reaches under the sheet) and "Set Up Project…"
/// for the project already open replaced it, and the first open's question
/// then read "Checking the project…" for good, its ticks lost with its Open.
#[test]
fn a_first_opens_sheet_is_not_replaced_by_another() {
    let data = test_dir("setup-data-held");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("config.toml"), "[llm]\napi_key = \"sk\"\n").unwrap();
    let _env = data_dir_override(&data);
    let mut app = scanned_app("setup-held-open");
    app.invalidate_llm_config();
    assert!(app.llm_available);
    let asked = fixture_project("setup-held-asked");
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        asked.clone(),
    ))));
    let census = Census {
        servers: vec![builds_and_downloads()],
        calls: Some(42),
    };
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root: asked.clone(),
        census: census.clone(),
    }));
    let _ = app.run_command_action(crate::keymap::Action::ExplainAll);
    let _ = app.update(Message::Project(ProjectMsg::SetupShow(SetupMode::Manage)));
    let setup = app.setup.as_ref().expect("the first open's sheet");
    assert_eq!((setup.mode, &setup.root), (SetupMode::Open, &asked));
    assert_eq!(setup.census.as_ref(), Some(&census));
    // Its Open carries out what it has ticked.
    let _ = app.update(Message::Project(ProjectMsg::ConsentAllowed(asked.clone())));
    let intent = app.pending_setup.as_ref().expect("the downloads wait");
    assert_eq!(intent.fetch, vec!["rust".to_string()]);
}

/// A first open's answer is for the project it opened. Another opened
/// instead while it scanned drops it, and so does a switch to another host:
/// kept, it ran — downloads and a paid Explain All pass, with nothing on
/// screen — the next time that path opened, here or on the other host.
#[test]
fn a_first_opens_answer_goes_when_its_project_does_not_open() {
    let data = test_dir("setup-data-dropped");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let mut app = blank_app();
    let answer = |app: &mut App, root: &PathBuf| {
        let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
            root.clone(),
        ))));
        let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
            root: root.clone(),
            census: Census {
                servers: vec![builds_and_downloads()],
                calls: Some(42),
            },
        }));
        let _ = app.update(Message::Project(ProjectMsg::ConsentAllowed(root.clone())));
        assert!(app.pending_setup.is_some(), "the downloads wait");
    };

    let first = fixture_project("setup-dropped-first");
    answer(&mut app, &first);
    // Another project opens before it does.
    scan_synchronously(&mut app, fixture_project("setup-dropped-instead"));
    assert!(app.pending_setup.is_none(), "kept for {first:?}");
    // Reopened later, trusted now: nothing is fetched behind the user's back.
    scan_synchronously(&mut app, first.clone());
    assert!(!app.proj.link.lsp.contains_key("rust"));

    let second = fixture_project("setup-dropped-second");
    answer(&mut app, &second);
    let _ = app.connect_to(crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    });
    assert!(app.pending_setup.is_none() && app.setup.is_none());
}

/// "Set Up Project…" asks for the servers the project lacks — among them
/// one whose download was declined, or failed, earlier: Apply fetches it.
/// It used to find the language's slot taken and do nothing, unsaid.
#[test]
fn set_up_project_fetches_a_server_once_declined() {
    let store = test_dir("setup-data-declined");
    let _ = std::fs::remove_dir_all(&store);
    let _env = data_dir_override(&store);
    let mut app = scanned_app("setup-declined");
    open_synchronously(&mut app, "src/lib.rs", None);
    let _ = app.update(Message::Lsp(LspMsg::ConsentDismissed(consent_asked(&app))));
    assert!(matches!(
        app.proj.link.lsp.get("rust"),
        Some(LspSlot::Declined(_))
    ));
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let _ = app.update(Message::Project(ProjectMsg::SetupShow(SetupMode::Manage)));
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root,
        census: Census {
            servers: vec![SetupServer {
                builds: false,
                ..builds_and_downloads()
            }],
            calls: Some(0),
        },
    }));
    let _ = app.update(Message::Project(ProjectMsg::SetupApply));
    assert!(
        matches!(app.proj.link.lsp.get("rust"), Some(LspSlot::Starting)),
        "the declined server was not fetched"
    );
    assert!(app.proj.pending_lsp_consent.is_empty());
}

/// A first open with the servers ticked, and a file of a ticked language
/// opened while the project's choices and state still load (a launch file,
/// a quick click): the server is fetched, and the file waits for it. Its
/// deferred start used to run first and ask the very download question the
/// tick had answered — after which the tick was skipped.
#[test]
fn a_first_opens_tick_answers_the_download_a_waiting_file_would_ask() {
    for state_first in [false, true] {
        let data = test_dir("setup-data-waiting");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        let _env = data_dir_override(&data);
        let root = fixture_project("setup-waiting");
        let mut app = blank_app();
        let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
            root.clone(),
        ))));
        let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
            root: root.clone(),
            census: Census {
                servers: vec![builds_and_downloads()],
                calls: Some(42),
            },
        }));
        let _ = app.update(Message::Project(ProjectMsg::ConsentAllowed(root.clone())));
        app.scanning = true;
        app.pending_scan_root = Some(root.clone());
        let _ = app.update(Message::Project(ProjectMsg::ScanDone {
            stamp: app.transport_stamp(),
            result: fs_scan::scan(root.clone()),
            report: Default::default(),
        }));
        open_synchronously(&mut app, "src/lib.rs", None);
        assert!(app.proj.inflight.deferred_lsp.iter().any(|l| l == "rust"));
        assert!(!app.proj.link.lsp.contains_key("rust"), "it waits");
        if state_first {
            let state = LoadedProjectState::read(
                &root,
                app.proj.derived_dir.as_deref(),
                app.local_project_state(),
            );
            let _ = app.update(Message::Project(ProjectMsg::StateLoaded {
                stamp: Stamp {
                    root: Some(root.clone()),
                    epoch: app.project_epoch,
                    conn: None,
                },
                state: Handoff::new(state),
            }));
            load_choices_synchronously(&mut app);
        } else {
            load_state_synchronously(&mut app);
        }
        assert!(app.pending_setup.is_none(), "carried out");
        assert!(
            app.proj.pending_lsp_consent.is_empty(),
            "asked again (state first: {state_first})"
        );
        assert!(
            matches!(app.proj.link.lsp.get("rust"), Some(LspSlot::Starting)),
            "not fetched (state first: {state_first})"
        );
    }
}

/// "Set Up Project…" lists a server in hand that builds the project, once
/// declined leave to build: Apply gives the leave and starts it, as the
/// Languages page's "Ask again" would. The leave used to be recorded and
/// the server left declined, not started until the project opened again.
#[test]
fn set_up_project_starts_a_server_once_declined_leave_to_build() {
    let root = fixture_project("setup-build-declined");
    let data = test_dir("setup-data-build-declined");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    fake_installed_rust_analyzer(&data);
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);
    let _ = app.update(Message::Lsp(LspMsg::ConsentDismissed(consent_asked(&app))));
    assert!(matches!(
        app.proj.link.lsp.get("rust"),
        Some(LspSlot::Declined(m)) if m.contains("not allowed to build")
    ));
    let _ = app.update(Message::Project(ProjectMsg::SetupShow(SetupMode::Manage)));
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root: root.clone(),
        census: Census {
            servers: vec![SetupServer {
                fetch: Fetch::Nothing,
                ..builds_and_downloads()
            }],
            calls: Some(0),
        },
    }));
    let _ = app.update(Message::Project(ProjectMsg::SetupApply));
    assert!(clew_core::trust::Trust::load().is_build_trusted(None, &root, "rust-analyzer"));
    assert!(
        matches!(app.proj.link.lsp.get("rust"), Some(LspSlot::Starting)),
        "allowed, it starts"
    );
    assert!(app.proj.pending_lsp_consent.is_empty(), "not asked again");
}
