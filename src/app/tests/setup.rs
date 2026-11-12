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
/// project lands in it; closing it (its ×, or Escape) opens nothing and
/// records nothing.
#[test]
fn a_first_open_asks_in_the_sheet_and_closing_it_opens_nothing() {
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

/// Not now opens the project — and does nothing else the sheet offered,
/// ticked or not: no leave to build, no downloads, no explaining. Each
/// server asks as ever when a file of its language opens.
#[test]
fn not_now_opens_the_project_with_nothing_set_up() {
    let data = test_dir("setup-data-not-now");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-not-now");
    let mut app = blank_app();
    app.llm_available = true;
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
    let _ = app.update(Message::Project(ProjectMsg::SetupExplain(true)));
    let setup = app.setup.as_ref().unwrap();
    assert!(setup.servers && setup.explain, "both ticked");
    let _ = app.update(Message::Project(ProjectMsg::OpenWithoutSetup(root.clone())));
    assert!(
        app.setup.is_none() && app.pending_consent.is_none(),
        "answered"
    );
    assert!(app.scanning, "the project opens");
    let trust = clew_core::trust::Trust::load();
    assert!(trust.is_root_trusted(None, &root), "not asked again");
    assert!(!trust.is_build_trusted(None, &root, "rust-analyzer"));
    let intent = app
        .pending_setup
        .as_ref()
        .expect("the answer, for the open");
    assert!(
        intent.fetch.is_empty() && !intent.explain,
        "nothing downloaded, nothing explained"
    );
    assert!(intent.servers_not_now, "and no server asked about");
}

/// Not now answers the sheet's question as Open does: pressed before the
/// sheet can have been read — a click meant for what was there a moment
/// before — it opens nothing; once it can have been, the project opens.
#[test]
fn not_now_waits_until_the_sheet_can_have_been_read() {
    let data = test_dir("setup-data-not-now-unread");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-not-now-unread");
    let mut app = blank_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        root.clone(),
    ))));
    let _ = app.update(Message::Noop);
    let _ = app.update(Message::Project(ProjectMsg::OpenWithoutSetup(root.clone())));
    assert!(
        app.pending_consent.is_some() && app.setup.is_some(),
        "not answered"
    );
    assert!(!app.scanning, "nothing opened");
    assert!(!clew_core::trust::Trust::load().is_root_trusted(None, &root));
    read_for(&mut app, delay);
    let _ = app.update(Message::Project(ProjectMsg::OpenWithoutSetup(root.clone())));
    assert!(app.pending_consent.is_none() && app.scanning, "opened");
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
    assert_eq!(
        intent
            .fetch
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>(),
        ["rust"]
    );
    assert!(!intent.explain);
}

/// Servers unticked: nothing recorded, nothing fetched — and none asked
/// about when a file of its language opens (see
/// `opened_without_setup_no_server_is_asked_about`).
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
    let intent = app
        .pending_setup
        .as_ref()
        .expect("the answer, for the open");
    assert!(intent.fetch.is_empty() && intent.servers_not_now);
}

/// Opened without setup (Not now, or the servers unticked), a file of the
/// language does not pop the question the sheet just had answered: its
/// server is left "not set up", for the status bar's Set up — and asked
/// about again once the user asks for it ("Ask again").
#[test]
fn opened_without_setup_no_server_is_asked_about() {
    let data = test_dir("setup-data-later");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-later");
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
    let _ = app.update(Message::Project(ProjectMsg::OpenWithoutSetup(root.clone())));
    app.scanning = true;
    app.pending_scan_root = Some(root.clone());
    let _ = app.update(Message::Project(ProjectMsg::ScanDone {
        stamp: app.transport_stamp(),
        result: fs_scan::scan(root.clone()),
        report: Default::default(),
    }));
    assert!(app.proj.servers_not_now);
    open_synchronously(&mut app, "src/lib.rs", None);
    load_state_synchronously(&mut app);
    assert!(app.pending_setup.is_none(), "carried out");
    assert!(app.proj.pending_lsp_consent.is_empty(), "not asked");
    assert!(
        matches!(app.proj.link.lsp.get("rust"), Some(LspSlot::Declined(_))),
        "{:?}",
        app.proj.link.lsp.get("rust")
    );
    // Asked for: the question comes, as ever — for this language; the
    // others stay left for later.
    let _ = app.update(Message::Lsp(LspMsg::DownloadFor("rust".into())));
    assert!(app.proj.servers_not_now && app.proj.servers_asked.contains("rust"));
    assert!(!app.server_left_for_later("rust"), "asked for");
    assert!(
        app.server_left_for_later("python"),
        "the others still for later"
    );
    assert!(
        !app.proj.pending_lsp_consent.is_empty()
            || matches!(app.proj.link.lsp.get("rust"), Some(LspSlot::Starting)),
        "{:?}",
        app.proj.link.lsp.get("rust")
    );
}

/// Open on a project the look found no server to set up for: nothing was
/// declined, so a server that turns up later (a file of another language,
/// a download it needs) is asked about as ever, not left "not set up".
#[test]
fn opening_with_nothing_to_set_up_declines_nothing() {
    let data = test_dir("setup-data-nothing");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let root = fixture_project("setup-nothing");
    let mut app = blank_app();
    app.llm_available = true;
    let _ = app.update(Message::Project(ProjectMsg::FolderPicked(Some(
        root.clone(),
    ))));
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root: root.clone(),
        census: Census {
            servers: vec![],
            calls: Some(42),
        },
    }));
    let _ = app.update(Message::Project(ProjectMsg::ConsentAllowed(root.clone())));
    assert!(app.setup.is_none(), "answered");
    assert!(
        app.pending_setup
            .as_ref()
            .is_none_or(|intent| !intent.servers_not_now),
        "nothing declined"
    );
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
    assert_eq!(
        intent
            .fetch
            .iter()
            .map(|(l, _)| l.as_str())
            .collect::<Vec<_>>(),
        ["rust"]
    );
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

/// A project whose language server is declined is offered, in one click,
/// what its servers still take: the status bar's Set up fetches it, says
/// it is setting up meanwhile, and is gone while it does. What a later
/// look finds replaces what an earlier one did — a look begun before
/// another is not taken.
#[test]
fn set_up_in_one_click_fetches_what_the_servers_lack() {
    let store = test_dir("setup-data-one-click");
    let _ = std::fs::remove_dir_all(&store);
    let _env = data_dir_override(&store);
    let mut app = scanned_app("setup-one-click");
    open_synchronously(&mut app, "src/lib.rs", None);
    let _ = app.update(Message::Lsp(LspMsg::ConsentDismissed(consent_asked(&app))));
    assert!(matches!(
        app.proj.link.lsp.get("rust"),
        Some(LspSlot::Declined(_))
    ));
    let stamp = app.stamp();
    let needs = SetupServer {
        builds: false,
        ..builds_and_downloads()
    };
    // Two looks: the older one's answer, landing last, is not taken.
    let _ = app.look_for_missing_setup();
    let older = app.proj.setup_look;
    let _ = app.look_for_missing_setup();
    let _ = app.update(Message::Project(ProjectMsg::MissingSetupFound {
        stamp: stamp.clone(),
        seq: app.proj.setup_look,
        servers: vec![needs.clone()],
    }));
    let _ = app.update(Message::Project(ProjectMsg::MissingSetupFound {
        stamp,
        seq: older,
        servers: Vec::new(),
    }));
    assert_eq!(app.missing_setup(), vec![&needs], "declined: offered");
    assert!(!app.setting_up());

    let _ = app.update(Message::Project(ProjectMsg::SetUpMissing));
    assert!(
        matches!(app.proj.link.lsp.get("rust"), Some(LspSlot::Starting)),
        "the declined server was not fetched"
    );
    assert!(app.missing_setup().is_empty(), "on its way: not offered");
    assert!(app.setting_up(), "and said to be setting up");
}

/// Set up looks afresh as it is clicked: a server another window put in
/// place since the last look — which nothing then fetches, and no fetch's
/// end takes off the status bar — is found in hand, and Set up goes.
#[test]
fn set_up_looks_afresh_for_what_is_now_in_hand() {
    let store = test_dir("setup-data-one-click-fresh");
    let _ = std::fs::remove_dir_all(&store);
    let _env = data_dir_override(&store);
    let mut app = scanned_app("setup-one-click-fresh");
    // A server that takes leave to build and nothing fetched: what the
    // click does needs no network. One to download made the task below a
    // real download of rust-analyzer, waited on whole — and it hung,
    // now and then, where the network did.
    let needs = SetupServer {
        fetch: Fetch::Nothing,
        ..builds_and_downloads()
    };
    app.proj.setup_missing = vec![needs];
    let before = app.proj.setup_look;
    let task = app.update(Message::Project(ProjectMsg::SetUpMissing));
    assert!(app.proj.setup_look > before, "no look begun");
    let looked = run_task(task).into_iter().any(|m| {
        matches!(
            m,
            Message::Project(ProjectMsg::MissingSetupFound { seq, .. }) if seq == app.proj.setup_look
        )
    });
    assert!(looked, "the look's answer is the one taken");
}

/// A server in hand that builds the project is offered until the leave to
/// build is given — by the one click, which gives it — and then no more.
/// One running, or being asked about, is not offered at all.
#[test]
fn a_build_once_allowed_is_not_offered_again() {
    let root = fixture_project("setup-one-click-build");
    let data = test_dir("setup-data-one-click-build");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let _env = data_dir_override(&data);
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    let builds = SetupServer {
        fetch: Fetch::Nothing,
        ..builds_and_downloads()
    };
    app.proj.setup_missing = vec![builds.clone()];
    assert_eq!(app.missing_setup(), vec![&builds]);
    app.proj
        .link
        .lsp
        .insert("rust".into(), LspSlot::AwaitingConsent);
    assert!(app.missing_setup().is_empty(), "being asked about");
    app.proj.link.lsp.remove("rust");
    let _ = app.update(Message::Project(ProjectMsg::SetUpMissing));
    assert!(clew_core::trust::Trust::load().is_build_trusted(None, &root, "rust-analyzer"));
    assert!(app.missing_setup().is_empty(), "allowed: nothing to offer");
}

/// A server whose own program is already on this computer is set up as
/// that program — chosen for the project, nothing installed — and the sheet
/// and the tour say so; with none on the PATH, Clew's copy is fetched as
/// before. The tour said "pyright: install with npm" with pyright on the
/// PATH.
#[cfg(unix)]
#[test]
fn a_server_on_this_computer_is_used_not_installed_again() {
    use crate::app::setup::{ServerLook, servers_line};
    use std::os::unix::fs::PermissionsExt;
    let store = test_dir("on-path-setup-store");
    let _ = std::fs::remove_dir_all(&store);
    let _env = data_dir_override(&store);
    let root = test_dir("on-path-setup-project");
    let bin = test_dir("on-path-setup-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("pyright-langserver");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let look = |path: &std::path::Path| ServerLook {
        path: Some(path.into()),
        ..Default::default()
    };
    let python = ["python".to_string()];

    let servers = look(&bin).servers_for(&root, &python);
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert_eq!(servers[0].fetch, Fetch::OnPath(program.clone()));
    assert_eq!(servers_line(&servers), "pyright: already installed");

    let empty = test_dir("on-path-setup-empty");
    std::fs::create_dir_all(&empty).unwrap();
    let servers = look(&empty).servers_for(&root, &python);
    assert_eq!(
        servers[0].fetch,
        Fetch::Install("npm".into()),
        "none here: Clew's copy"
    );
}

/// Enter on a sheet presses its main button, as its "yes" by voice does:
/// the cost sheet Explain All's shortcut opens took no Enter, the ring
/// having nothing on it. Not before the button can be pressed — while the
/// project is still being looked at — and not on a question of trust.
#[test]
fn enter_on_a_sheet_presses_its_main_button() {
    let data = test_dir("setup-data-enter");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "[llm]\napi_key = \"sk\"\nbase_url = \"http://127.0.0.1:9/v1\"\n",
    )
    .unwrap();
    let _env = data_dir_override(&data);
    let mut app = scanned_app("setup-enter");
    app.invalidate_llm_config();
    assert!(app.llm_available);
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let enter = || {
        key_press(
            keyboard::Key::Named(keyboard::key::Named::Enter),
            keyboard::Modifiers::default(),
        )
    };
    let _ = app.show_setup(SetupMode::Explain, root.clone());
    // Still counting: Explain cannot be pressed, and Enter does nothing.
    let _ = app.update(enter());
    assert!(app.setup.is_some(), "pressed before it could be");
    let _ = app.update(Message::Project(ProjectMsg::SetupCounted {
        root,
        census: Census {
            servers: Vec::new(),
            calls: Some(10),
        },
    }));
    assert!(app.setup.is_some());
    let _ = app.update(enter());
    assert!(app.setup.is_none(), "Enter pressed Explain");
    assert!(app.proj.explain.running, "and explaining began");
}

/// Enter allows no question of trust: a language server's download, put
/// on screen as a file opens, stays asked after Enter — only its own
/// Allow (or "yes" said to it) allows it.
#[test]
fn enter_allows_no_question_of_trust() {
    let store = test_dir("setup-enter-trust-store");
    let _ = std::fs::remove_dir_all(&store);
    let _env = data_dir_override(&store);
    let mut app = scanned_app("setup-enter-trust");
    open_synchronously(&mut app, "src/lib.rs", None);
    assert!(!app.proj.pending_lsp_consent.is_empty(), "asked");
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::Enter),
        keyboard::Modifiers::default(),
    ));
    assert!(!app.proj.pending_lsp_consent.is_empty(), "allowed by Enter");
    assert!(matches!(
        app.proj.link.lsp.get("rust"),
        Some(LspSlot::AwaitingConsent)
    ));
}
