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
    // Said by its page, not the status line too: there it stood on after
    // the page was put away for another ("No weak spots" over the
    // glossary).
    assert!(!app.status.contains("weak spot"), "{}", app.status);
    // Shown again, nothing changed: not measured again.
    let _ = app.update(Message::Weak(WeakMsg::Close));
    let _ = app.update(Message::Weak(WeakMsg::Show));
    assert!(!app.proj.weak.building, "measured again for nothing");

    // A pass that lands with its page put away is said on the status line.
    app.proj.weak.spots = None;
    let task = app.update(Message::Weak(WeakMsg::Show));
    assert!(app.proj.weak.building);
    let _ = app.update(Message::Weak(WeakMsg::Close));
    for m in run_task(task) {
        let _ = app.update(m);
    }
    assert!(app.status.contains("1 weak spot"), "{}", app.status);
    let _ = app.update(Message::Weak(WeakMsg::Show));

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

/// The overview's architecture — read from the project, its imports among
/// them — is on the page a project opens on before any overview is written,
/// its parts linked as they import one another; and read on Show when it is
/// not yet — let go while the page is put away and the graph moves on.
#[test]
fn the_overview_shows_its_architecture_before_any_prose() {
    let mut app = scanned_app("overview-map-first");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    for dir in ["src/web", "src/data"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for (rel, text) in [
        ("src/web/__init__.py", ""),
        ("src/web/views.py", "from ..data import orders\n"),
        ("src/data/__init__.py", ""),
        ("src/data/orders.py", "X = 1\n"),
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
    let arch = app
        .proj
        .overview
        .architecture
        .clone()
        .expect("the architecture on the landing page");
    let at = |name: &str| arch.components.iter().position(|c| c.name == name);
    let (web, data) = (at("web").expect("web"), at("data").expect("data"));
    assert!(
        arch.links.iter().any(|l| l.from == web && l.to == data),
        "{arch:?}"
    );
    // Put away, the graph moving on: let go. Show reads it again.
    let _ = app.update(Message::Overview(OverviewMsg::Close));
    app.refresh_architecture();
    assert!(app.proj.overview.architecture.is_none(), "let go unseen");
    let _ = app.update(Message::Overview(OverviewMsg::Show));
    assert!(app.proj.overview.architecture.is_some(), "read on Show");
}

/// The architecture of the project `ARCH_PROBE` names, read as the app
/// reads it — scanned, indexed, its imports resolved — printed. Not a test
/// of anything: a way to look at a real project's. Without it, it passes.
#[test]
#[ignore = "needs ARCH_PROBE"]
fn probe_a_projects_architecture() {
    let Some(root) = std::env::var_os("ARCH_PROBE").map(std::path::PathBuf::from) else {
        eprintln!("skipped: ARCH_PROBE is not set");
        return;
    };
    let mut app = blank_app();
    scan_synchronously(&mut app, root.canonicalize().unwrap());
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let files = app.proj.project.as_ref().unwrap().files.clone();
    let started = std::time::Instant::now();
    let indexed = app.update(Message::Watch(WatchMsg::SymbolIndexDone {
        stamp: app.stamp(),
        indexed: index::build_indexed(&root, files),
    }));
    settle_imports(&mut app, indexed);
    let read = std::time::Instant::now();
    let arch = app.compute_architecture().expect("a project");
    eprintln!(
        "{:?}: {} components, {} links, read in {:?} (indexed in {:?})",
        arch.basis,
        arch.components.len(),
        arch.links.len(),
        read.elapsed(),
        read - started
    );
    for (i, c) in arch.components.iter().enumerate() {
        eprintln!(
            "[{i}] {} ({}) {:?} {:?} {} files{} — {:?}\n     parts: {:?} +{}\n     entry: {:?} on: {:?}",
            c.name,
            c.rel,
            c.kind,
            c.language,
            c.files,
            if c.members > 1 {
                format!(", {} packages", c.members)
            } else {
                String::new()
            },
            c.about,
            c.parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            c.more_parts,
            c.entry,
            c.built_on
        );
    }
    for l in &arch.links {
        eprintln!(
            "  {} -> {} ({})",
            arch.components[l.from].name, arch.components[l.to].name, l.imports
        );
    }
    eprintln!("rows: {:?}", crate::architecture::layers(&arch));
}
