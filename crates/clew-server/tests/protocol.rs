//! End-to-end integration test for the client↔server protocol.
//!
//! Drives a `Server` in-process over a temp project through the core request
//! flow — open, read, search, docs, path-confinement, list-dir — asserting the
//! replies. This covers the architecture seam (the protocol contract) that the
//! GUI/SSH paths only exercise manually.

use clew_protocol::{AiEndpoint, Event, PROTOCOL_VERSION, Request, ServerMessage, TargetSpec};
use clew_server::Server;
use std::path::PathBuf;
use tokio::sync::mpsc;

/// A throwaway project on disk with a couple of documented source files.
fn temp_project(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clew-server-it-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// Adds two numbers.\npub fn add(a: i32, b: i32) -> i32 { a + b }\n\nfn helper() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/util.py"),
        "class Greeter:\n    def hello(self):\n        pass\n",
    )
    .unwrap();
    dir
}

fn host_target() -> TargetSpec {
    TargetSpec {
        label: "test".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        family: "unix".into(),
    }
}

#[tokio::test]
async fn protocol_round_trip() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("roundtrip");
    let root_str = root.to_string_lossy().into_owned();

    // Hello → Ready (agreed protocol version).
    let ready = server
        .handle(Request::Hello {
            protocol: PROTOCOL_VERSION,
            ai: AiEndpoint::Server,
        })
        .await;
    assert!(
        matches!(ready, Some(Event::Ready { .. })),
        "expected Ready, got {ready:?}"
    );

    // OpenProject → Tree with the flat file list.
    let files = match server
        .handle(Request::OpenProject {
            root: root_str.clone(),
        })
        .await
    {
        Some(Event::Tree { files, .. }) => files,
        other => panic!("expected Tree, got {other:?}"),
    };
    assert!(files.iter().any(|f| f == "src/lib.rs"), "lib.rs in tree");
    assert!(files.iter().any(|f| f == "src/util.py"), "util.py in tree");

    // ReadFile → highlighted content + outline symbols + doc comments.
    match server
        .handle(Request::ReadFile {
            rel: "src/lib.rs".into(),
            target: host_target(),
        })
        .await
    {
        Some(Event::FileContent {
            source,
            lines,
            symbols,
            docs,
            ..
        }) => {
            assert!(source.contains("pub fn add"));
            assert!(!lines.is_empty(), "highlighted lines present");
            assert!(symbols.iter().any(|s| s.name == "add"), "outline has add");
            assert!(
                docs.iter().any(|(_, d)| d.contains("Adds two numbers")),
                "doc comment extracted"
            );
        }
        other => panic!("expected FileContent, got {other:?}"),
    }

    // Search → hits (project-wide grep).
    match server
        .handle(Request::Search {
            query: "helper".into(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            include: String::new(),
            exclude: String::new(),
        })
        .await
    {
        Some(Event::SearchResults { hits, error }) => {
            assert!(error.is_none(), "no search error: {error:?}");
            assert!(
                hits.iter().any(|h| h.rel == "src/lib.rs"),
                "found helper in lib.rs"
            );
        }
        other => panic!("expected SearchResults, got {other:?}"),
    }

    // BuildDocs runs on a detached task and replies immediately with no direct
    // result; the per-file API index arrives as a Docs notification.
    assert!(
        server.handle(Request::BuildDocs).await.is_none(),
        "BuildDocs replies async"
    );
    let files = loop {
        match rx.recv().await.expect("a server message") {
            ServerMessage::Notification {
                event: Event::Docs { files },
                ..
            } => break files,
            _ => continue, // skip any unrelated notifications
        }
    };
    let lib = files
        .iter()
        .find(|f| f.rel == "src/lib.rs")
        .expect("lib.rs docs");
    let add = lib
        .items
        .iter()
        .find(|i| i.name == "add")
        .expect("add doc item");
    assert!(add.public, "add is public API");
    assert!(add.doc.contains("Adds two numbers"), "add carries its doc");
    // Python method nests under its class.
    let py = files
        .iter()
        .find(|f| f.rel == "src/util.py")
        .expect("util.py docs");
    let cls = py
        .items
        .iter()
        .find(|i| i.name == "Greeter")
        .expect("Greeter");
    assert!(
        cls.children.iter().any(|c| c.name == "hello"),
        "hello nests under Greeter"
    );
}

#[tokio::test]
async fn read_file_refuses_path_traversal() {
    let (tx, _rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("confine");
    server
        .handle(Request::OpenProject {
            root: root.to_string_lossy().into_owned(),
        })
        .await;

    // A path escaping the project must be refused, not read.
    let escaped = server
        .handle(Request::ReadFile {
            rel: "../../../../etc/passwd".into(),
            target: host_target(),
        })
        .await;
    assert!(
        matches!(escaped, Some(Event::Error { .. })),
        "path traversal must be refused, got {escaped:?}"
    );
}

#[tokio::test]
async fn list_dir_lists_the_host() {
    let (tx, _rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("listdir");

    match server
        .handle(Request::ListDir {
            path: Some(root.to_string_lossy().into_owned()),
        })
        .await
    {
        Some(Event::DirListing { entries, .. }) => {
            assert!(
                entries.iter().any(|e| e.name == "src" && e.is_dir),
                "src dir listed as a directory"
            );
        }
        other => panic!("expected DirListing, got {other:?}"),
    }
}
