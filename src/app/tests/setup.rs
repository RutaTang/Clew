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
