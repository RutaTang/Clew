//! The weak spots page (`app::weakspots`) and the overview's module map
//! before any prose: measured where the files are, ranked, opened, asked
//! about.

use super::*;
use crate::WeakMsg;

/// A Rust file with one risky function and one fine one.
const RISKY: &str = "pub fn load(path: &str) -> String {\n    // FIXME: say which file is missing\n    std::fs::read_to_string(path).unwrap()\n}\n\npub fn fine(a: u32) -> u32 {\n    a + 1\n}\n";

/// The page measures a local project's files where they are: the risky
/// function is listed, with why, the fine one not; the other pages make
/// way for it, and a pass for an older file set is dropped.
#[test]
fn the_page_lists_a_local_projects_weak_spots() {
    let mut app = scanned_app("weak-local");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(root.join("src/load.rs"), RISKY).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("tests/it.rs"), RISKY).unwrap();
    scan_synchronously(&mut app, root.clone());
    app.proj.stats.showing = true;

    let task = app.update(Message::Weak(WeakMsg::Show));
    assert!(app.proj.weak.showing);
    assert!(!app.proj.stats.showing, "one page at a time");
    assert!(app.proj.weak.building);
    let built: Vec<Message> = run_task(task);
    // A pass for files that have since changed lands nowhere.
    let mut stale = built.clone();
    if let Some(Message::Weak(WeakMsg::Built { rev, .. })) = stale.first_mut() {
        *rev += 1;
    }
    for m in stale {
        let _ = app.update(m);
    }
    assert!(app.proj.weak.spots.is_none(), "another pass's spots taken");
    for m in built {
        let _ = app.update(m);
    }
    assert!(!app.proj.weak.building);
    let spots = app.proj.weak.spots.clone().expect("measured");
    assert_eq!(spots.len(), 1, "{spots:?}");
    assert_eq!(spots[0].name, "load");
    assert_eq!(spots[0].rel, "src/load.rs", "a test file is no weak spot");
    assert!(app.status.contains("1 weak spot"), "{}", app.status);
    // Shown again, nothing changed: not measured again.
    let _ = app.update(Message::Weak(WeakMsg::Close));
    let _ = app.update(Message::Weak(WeakMsg::Show));
    assert!(!app.proj.weak.building, "measured again for nothing");

    // Its row opens its code, putting the page away.
    let _ = app.update(Message::Weak(WeakMsg::Open(0)));
    assert!(!app.proj.weak.showing);
    // Ask why: the question names it, where it is, and why it is listed.
    app.llm_config_cache = Some(Some(unreachable_llm()));
    let _rx = attach_server(&mut app);
    let _ = app.update(Message::Weak(WeakMsg::AskWhy(0)));
    let asked = app
        .proj
        .ask_turns
        .last()
        .map(|t| t.question.clone())
        .expect("a question was asked");
    assert!(asked.contains("load"), "{asked}");
    assert!(asked.contains("src/load.rs:1"), "{asked}");
    assert!(asked.contains("1 unwrap"), "{asked}");
}

/// A remote project's files are measured as its host has them, read over
/// `ReadSources`: none of this machine's same-pathed files is read.
#[test]
fn a_remote_projects_weak_spots_are_read_from_its_host() {
    let (mut app, root, mut rx) = remote_app("weak-remote");
    // This machine's copy holds nothing risky: the host's does.
    std::fs::write(root.join("src/lib.rs"), "pub fn fine() {}\n").unwrap();
    let task = app.update(Message::Weak(WeakMsg::Show));
    let pending = app.server.ai_pending.clone();
    let root_s = root.to_string_lossy().into_owned();
    let running = std::thread::spawn(move || run_task(task));
    let mut asked = Vec::new();
    while !running.is_finished() {
        while let Ok(msg) = rx.try_recv() {
            let clew_protocol::ClientMessage { id, request } = msg;
            let clew_protocol::Request::ReadSources { rels } = request else {
                continue;
            };
            asked.extend(rels.clone());
            let reply = clew_protocol::Event::Sources {
                root: root_s.clone(),
                files: rels.into_iter().map(|r| (r, RISKY.to_string())).collect(),
                missing: Vec::new(),
                too_large: Vec::new(),
                refused: Vec::new(),
                unreadable: Vec::new(),
                deferred: Vec::new(),
            };
            if let Some(tx) = crate::app::rpc::lock_replies(&pending).remove(&id) {
                let _ = tx.send(reply);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for m in running.join().unwrap() {
        let _ = app.update(m);
    }
    assert_eq!(asked, ["src/lib.rs"]);
    let spots = app.proj.weak.spots.clone().expect("measured");
    assert_eq!(
        spots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["load"],
        "the host's file, not this machine's"
    );
}

/// The overview's module map — the architecture, drawn from the imports —
/// is on the page a project opens on before any overview is written, and
/// laid out on Show when it is not yet.
#[test]
fn the_overview_shows_its_module_map_before_any_prose() {
    let mut app = scanned_app("overview-map-first");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::create_dir_all(root.join("py")).unwrap();
    for (rel, text) in [
        ("py/main.py", "import a\nimport b\nimport c\n"),
        ("py/a.py", "import b\nimport c\n"),
        ("py/b.py", "import c\n"),
        ("py/c.py", "X = 1\n"),
    ] {
        std::fs::write(root.join(rel), text).unwrap();
    }
    scan_synchronously(&mut app, root.clone());
    let files = app.proj.project.as_ref().unwrap().files.clone();
    let indexed = app.update(Message::Watch(WatchMsg::SymbolIndexDone {
        stamp: app.stamp(),
        indexed: index::build_indexed(&root, files),
    }));
    settle_imports(&mut app, indexed);
    assert!(app.proj.overview.markdown.is_none(), "no prose written");
    assert!(app.proj.overview.showing, "the page a project opens on");
    let map = app
        .proj
        .overview
        .map
        .as_ref()
        .expect("a map on the landing page");
    assert!(map.nodes.len() >= 3, "{}", map.nodes.len());
    // Put away and the map dropped: Show lays it out again.
    let _ = app.update(Message::Overview(OverviewMsg::Close));
    app.proj.overview.map = None;
    let _ = app.update(Message::Overview(OverviewMsg::Show));
    assert!(app.proj.overview.map.is_some(), "laid out on Show");
}
