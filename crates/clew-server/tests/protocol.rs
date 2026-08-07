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

/// Wait for the correlated `Reply` to request `id`, skipping notifications.
/// Slow arms (read, search, blame, AI) answer from a detached task via the
/// out channel, so the request loop never stalls behind them.
async fn recv_reply(rx: &mut mpsc::UnboundedReceiver<ServerMessage>, id: u64) -> Event {
    loop {
        match rx.recv().await.expect("a server message") {
            ServerMessage::Reply { id: got, event, .. } if got == id => break event,
            _ => continue,
        }
    }
}

/// Open a project and wait for its (now asynchronous) Tree reply.
async fn open_project(
    server: &mut Server,
    rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    id: u64,
    root: &PathBuf,
) -> Vec<String> {
    assert!(
        server
            .handle(
                id,
                Request::OpenProject {
                    root: root.to_string_lossy().into_owned(),
                },
            )
            .await
            .is_none(),
        "OpenProject replies async"
    );
    match recv_reply(rx, id).await {
        Event::Tree { files, .. } => files,
        other => panic!("expected Tree, got {other:?}"),
    }
}

#[tokio::test]
async fn protocol_round_trip() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("roundtrip");

    // Hello → Ready (agreed protocol version).
    let ready = server
        .handle(
            1,
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                ai: AiEndpoint::Server,
            },
        )
        .await;
    assert!(
        matches!(ready, Some(Event::Ready { .. })),
        "expected Ready, got {ready:?}"
    );

    // OpenProject → Tree with the flat file list (an async reply: the scan
    // runs off the request loop).
    let files = open_project(&mut server, &mut rx, 2, &root).await;
    assert!(files.iter().any(|f| f == "src/lib.rs"), "lib.rs in tree");
    assert!(files.iter().any(|f| f == "src/util.py"), "util.py in tree");

    // ReadFile answers asynchronously: the reply arrives on the out channel,
    // correlated by id, so a slow highlight can't stall the request loop.
    assert!(
        server
            .handle(
                3,
                Request::ReadFile {
                    rel: "src/lib.rs".into(),
                    target: host_target(),
                }
            )
            .await
            .is_none(),
        "ReadFile replies async"
    );
    match recv_reply(&mut rx, 3).await {
        Event::FileContent {
            source,
            lines,
            symbols,
            docs,
            ..
        } => {
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

    // Search → hits (project-wide grep), also answered asynchronously.
    assert!(
        server
            .handle(
                4,
                Request::Search {
                    query: "helper".into(),
                    regex: false,
                    case_sensitive: false,
                    whole_word: false,
                    include: String::new(),
                    exclude: String::new(),
                }
            )
            .await
            .is_none(),
        "Search replies async"
    );
    match recv_reply(&mut rx, 4).await {
        Event::SearchResults { hits, error } => {
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
        server.handle(5, Request::BuildDocs).await.is_none(),
        "BuildDocs replies async"
    );
    let files = loop {
        match rx.recv().await.expect("a server message") {
            ServerMessage::Notification {
                event: Event::Docs { files, .. },
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
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("confine");
    open_project(&mut server, &mut rx, 1, &root).await;

    // A path escaping the project must be refused, not read. The refusal is
    // synchronous — confinement is checked before any work is spawned.
    let escaped = server
        .handle(
            2,
            Request::ReadFile {
                rel: "../../../../etc/passwd".into(),
                target: host_target(),
            },
        )
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
        .handle(
            1,
            Request::ListDir {
                path: Some(root.to_string_lossy().into_owned()),
            },
        )
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

/// The watcher refreshes the server's shared file list after a structural
/// change, so search greps the current file set — not the one from the last
/// `OpenProject`.
#[tokio::test]
async fn search_sees_files_created_after_open() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("watch-files");
    open_project(&mut server, &mut rx, 1, &root).await;

    // A file appears after the open (as if created by a build or an editor).
    std::fs::write(root.join("src/fresh.rs"), "fn brand_new_needle() {}\n").unwrap();

    // Wait for the watcher's rescan (debounced) to land: the Tree notification
    // is sent after the shared file list has been refreshed.
    let saw_tree = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match rx.recv().await.expect("a server message") {
                ServerMessage::Notification {
                    event: Event::Tree { files, .. },
                    ..
                } if files.iter().any(|f| f == "src/fresh.rs") => break,
                _ => continue,
            }
        }
    })
    .await;
    assert!(saw_tree.is_ok(), "watcher never reported the new file");

    // The new file is now searchable without re-opening the project.
    assert!(
        server
            .handle(
                2,
                Request::Search {
                    query: "brand_new_needle".into(),
                    regex: false,
                    case_sensitive: false,
                    whole_word: false,
                    include: String::new(),
                    exclude: String::new(),
                }
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 2).await {
        Event::SearchResults { hits, .. } => {
            assert!(
                hits.iter().any(|h| h.rel == "src/fresh.rs"),
                "search must see the watcher-refreshed file list"
            );
        }
        other => panic!("expected SearchResults, got {other:?}"),
    }
}

/// A repo-specified LSP `command` runs only when the client pushed a matching
/// approval — the gate every spawn path shares. Without one, SpawnLsp refuses;
/// after `LspApprovals` with the fingerprint from `LspResolve`, it spawns.
#[tokio::test]
async fn repo_lsp_command_needs_a_pushed_approval() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("lsp-approval");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    // A benign script standing in for the repo's command.
    let script = root.join("fake-lsp.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"fake-lsp.sh\"\n",
    )
    .unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;

    // Unapproved: refused with an error, and the proxy sees EOF. The spawn
    // resolves off the request loop, so the refusal arrives as a Reply.
    assert!(
        server
            .handle(
                2,
                Request::SpawnLsp {
                    proc: 7,
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    let refused = recv_reply(&mut rx, 2).await;
    assert!(
        matches!(refused, Event::Error { ref message } if message.contains("not approved")),
        "unapproved command must be refused, got {refused:?}"
    );

    // LspResolve reports what would run — command line + fingerprint.
    assert!(
        server
            .handle(
                3,
                Request::LspResolve {
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    let spec = match recv_reply(&mut rx, 3).await {
        Event::LspResolved {
            resolution: clew_protocol::LspResolution::Command(spec),
            ..
        } => spec,
        other => panic!("expected a resolved command, got {other:?}"),
    };
    assert!(spec.command.contains("fake-lsp.sh"));

    // Push the approval; the same spawn now proceeds (the script exits at
    // once, so the proxy reports ProcessExited rather than an Error).
    server
        .handle(
            4,
            Request::LspApprovals {
                approvals: vec![("rust".into(), spec.fingerprint.clone())],
            },
        )
        .await;
    let spawned = server
        .handle(
            5,
            Request::SpawnLsp {
                proc: 8,
                language: "rust".into(),
            },
        )
        .await;
    assert!(
        spawned.is_none(),
        "approved command must spawn, got {spawned:?}"
    );
    // Drain until the spawned process exits — proof it actually ran.
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let ServerMessage::Notification {
                event: Event::ProcessExited { proc: 8, .. },
                ..
            } = rx.recv().await.expect("a server message")
            {
                break;
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "approved spawn never ran");

    // A different project (OpenProject) clears the pushed approvals.
    let other = temp_project("lsp-approval-b");
    open_project(&mut server, &mut rx, 6, &other).await;
    open_project(&mut server, &mut rx, 7, &root).await;
    assert!(
        server
            .handle(
                8,
                Request::SpawnLsp {
                    proc: 9,
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    assert!(
        matches!(recv_reply(&mut rx, 8).await, Event::Error { .. }),
        "approvals must not survive a project switch"
    );
}

/// `SpawnLsp` for a store-managed server that is NOT installed must refuse —
/// never download. Installs happen only on `LspInstall`, the request that
/// carries the user's consent; `LspResolve` reports the install as pending so
/// the client can raise that consent prompt.
#[tokio::test]
// The env lock must span the whole test (CLEW_DATA_DIR stays overridden),
// and the single-threaded test runtime makes holding it across awaits fine.
#[allow(clippy::await_holding_lock)]
async fn spawn_lsp_never_installs_without_consent() {
    let _env = clew_core::env_lock();
    let data = std::env::temp_dir().join("clew-server-it-no-autoinstall-data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // SAFETY: env mutation serialized by env_lock.
    unsafe { std::env::set_var("CLEW_DATA_DIR", &data) };

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("no-autoinstall");
    open_project(&mut server, &mut rx, 1, &root).await;

    // No lsp.toml: "rust" resolves to the store-managed registry default,
    // which is not installed in this empty data dir.
    assert!(
        server
            .handle(
                2,
                Request::SpawnLsp {
                    proc: 7,
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    let refused = recv_reply(&mut rx, 2).await;
    assert!(
        matches!(refused, Event::Error { ref message } if message.contains("not installed")),
        "an uninstalled server must refuse to spawn, got {refused:?}"
    );
    // …and nothing was downloaded behind the user's back.
    assert!(
        !data.join("servers").exists(),
        "SpawnLsp must not install anything"
    );

    // LspResolve reports the pending install instead, for the consent prompt.
    assert!(
        server
            .handle(
                3,
                Request::LspResolve {
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 3).await {
        Event::LspResolved {
            resolution: clew_protocol::LspResolution::NeedsInstall { server, .. },
            ..
        } => assert_eq!(server, "rust-analyzer"),
        other => panic!("expected NeedsInstall, got {other:?}"),
    }

    unsafe { std::env::remove_var("CLEW_DATA_DIR") };
}

/// The Ask agent's semantic tools go through the same gate: an unapproved
/// repo command is refused, not executed.
#[tokio::test]
async fn agent_lsp_pool_honors_the_approval_gate() {
    let root = temp_project("agent-lsp-gate");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    let script = root.join("payload.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"payload.sh\"\n",
    )
    .unwrap();

    let approvals = clew_server::SharedApprovals::default();
    let pool = clew_server::agent_lsp::LspPool::new(root.clone(), approvals.clone());
    let stop = std::sync::atomic::AtomicBool::new(false);
    let refused = pool
        .query(
            clew_server::agent_lsp::Semantic::Definition,
            "src/lib.rs",
            &root.join("src/lib.rs"),
            2,
            "add",
            &stop,
        )
        .await;
    let err = match refused {
        Err(e) => e,
        Ok(_) => panic!("unapproved command must not run"),
    };
    assert!(err.contains("not approved"), "{err}");
}

/// A version mismatch is refused at the handshake with an explanatory error.
/// Silently proceeding used to fail far later and far more confusingly: the
/// peer's frames simply didn't deserialize and vanished.
#[tokio::test]
async fn hello_refuses_a_protocol_mismatch() {
    let (tx, _rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let refused = server
        .handle(
            1,
            Request::Hello {
                protocol: PROTOCOL_VERSION - 1,
                ai: AiEndpoint::Server,
            },
        )
        .await;
    match refused {
        Some(Event::Error { message }) => {
            assert!(message.contains("protocol mismatch"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Fail closed: a client that pipelined requests behind its Hello gets a
    // refusal for each — not best-effort answers on a connection it
    // half-understands.
    let pipelined = server
        .handle(
            2,
            Request::OpenProject {
                root: "/tmp".into(),
            },
        )
        .await;
    assert!(
        matches!(pipelined, Some(Event::Error { ref message }) if message.contains("handshake")),
        "requests after a failed handshake must be refused, got {pipelined:?}"
    );
    // The matching version still shakes hands (and reopens the connection).
    assert!(matches!(
        server
            .handle(
                3,
                Request::Hello {
                    protocol: PROTOCOL_VERSION,
                    ai: AiEndpoint::Server,
                }
            )
            .await,
        Some(Event::Ready { .. })
    ));
}

/// A spawned child that never reads its stdin must not wedge the request
/// loop: input is handed to a per-process writer task, so the pipe filling up
/// blocks nothing, and a following ProcessKill still gets through.
#[tokio::test]
async fn process_input_to_a_stalled_child_never_blocks_the_loop() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("stdin-backpressure");
    open_project(&mut server, &mut rx, 1, &root).await;

    // `sleep` never reads stdin, so its pipe fills and stays full.
    assert!(
        server
            .handle(
                2,
                Request::SpawnProcess {
                    proc: 3,
                    cmd: "sleep".into(),
                    args: vec!["30".into()],
                    cwd: None,
                },
            )
            .await
            .is_none(),
        "spawn should succeed"
    );

    // Far more than a pipe buffer (64 KiB is typical), in many requests.
    let chunk = vec![b'x'; 64 * 1024];
    let pumped = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for i in 0..16 {
            server
                .handle(
                    100 + i,
                    Request::ProcessInput {
                        proc: 3,
                        data: chunk.clone(),
                    },
                )
                .await;
        }
    })
    .await;
    assert!(pumped.is_ok(), "ProcessInput blocked the request loop");

    // And the loop is still live: the kill goes through and the child exits.
    server.handle(200, Request::ProcessKill { proc: 3 }).await;
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let ServerMessage::Notification {
                event: Event::ProcessExited { proc: 3, .. },
                ..
            } = rx.recv().await.expect("a server message")
            {
                break;
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "the kill never took effect");

    // The process table dropped the entry (no accumulation across a session).
    assert!(
        server
            .handle(
                201,
                Request::ProcessInput {
                    proc: 3,
                    data: vec![b'y'],
                },
            )
            .await
            .is_none(),
        "input to a gone process is a harmless no-op"
    );
}

/// Switching projects must not leave the previous project's language servers
/// or debug adapters running: OpenProject sweeps the whole process table (and
/// reports each death), in the same handler turn that switches the root.
#[tokio::test]
async fn open_project_kills_the_previous_projects_processes() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root_a = temp_project("switch-kill-a");
    open_project(&mut server, &mut rx, 1, &root_a).await;
    assert!(
        server
            .handle(
                2,
                Request::SpawnProcess {
                    proc: 6,
                    cmd: "sleep".into(),
                    args: vec!["30".into()],
                    cwd: None,
                },
            )
            .await
            .is_none()
    );

    // Switch to project B; the old project's process must die without any
    // ProcessKill from the client.
    let root_b = temp_project("switch-kill-b");
    assert!(
        server
            .handle(
                3,
                Request::OpenProject {
                    root: root_b.to_string_lossy().into_owned(),
                },
            )
            .await
            .is_none()
    );
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let ServerMessage::Notification {
                event: Event::ProcessExited { proc: 6, .. },
                ..
            } = rx.recv().await.expect("a server message")
            {
                break;
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "the project switch must kill old processes");
}

/// The client pipelines `ProcessInput` (an LSP `initialize`) right behind
/// `SpawnLsp`, whose resolve + spawn run on a detached task. Input sent in
/// that window must buffer and reach the child's stdin once it exists — the
/// stream starts at byte zero, never mid-way.
#[tokio::test]
async fn input_pipelined_behind_spawn_lsp_reaches_the_child() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("spawn-race");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    // `cat` as a stand-in LSP: echoes stdin, so output proves delivery.
    let script = root.join("echo-lsp.sh");
    std::fs::write(&script, "#!/bin/sh\nexec cat\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"echo-lsp.sh\"\n",
    )
    .unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        server
            .handle(
                2,
                Request::LspResolve {
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    let spec = match recv_reply(&mut rx, 2).await {
        Event::LspResolved {
            resolution: clew_protocol::LspResolution::Command(spec),
            ..
        } => spec,
        other => panic!("expected a resolved command, got {other:?}"),
    };
    server
        .handle(
            3,
            Request::LspApprovals {
                approvals: vec![("rust".into(), spec.fingerprint)],
            },
        )
        .await;

    // Spawn, then write immediately — while the resolve/spawn task is still
    // in flight. Nothing is awaited in between.
    assert!(
        server
            .handle(
                4,
                Request::SpawnLsp {
                    proc: 11,
                    language: "rust".into(),
                },
            )
            .await
            .is_none()
    );
    assert!(
        server
            .handle(
                5,
                Request::ProcessInput {
                    proc: 11,
                    data: b"Content-Length: 2\r\n\r\n{}".to_vec(),
                },
            )
            .await
            .is_none()
    );

    // The spawn acks, and the pipelined bytes come back out of `cat` intact.
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut started = false;
        let mut output = Vec::new();
        loop {
            match rx.recv().await.expect("a server message") {
                ServerMessage::Notification {
                    event: Event::ProcessStarted { proc: 11 },
                    ..
                } => started = true,
                ServerMessage::Notification {
                    event: Event::ProcessOutput { proc: 11, data },
                    ..
                } => {
                    assert!(started, "output before the ProcessStarted ack");
                    output.extend_from_slice(&data);
                    if output.len() >= 23 {
                        break output;
                    }
                }
                ServerMessage::Notification {
                    event: Event::ProcessExited { proc: 11, .. },
                    ..
                } => panic!("child died before echoing: got {output:?}"),
                _ => continue,
            }
        }
    })
    .await
    .expect("pipelined input never came back — dropped in the spawn race");
    assert_eq!(echoed, b"Content-Length: 2\r\n\r\n{}");
    server.handle(6, Request::ProcessKill { proc: 11 }).await;
}

/// A spawn that fails at the OS level must end the stream like any other
/// death: `ProcessExited` (so the client's proxy sees EOF and unmaps the
/// handle) plus the error itself — never the error alone.
#[tokio::test]
async fn failed_spawn_reports_process_exited() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("spawn-fail");
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        server
            .handle(
                2,
                Request::SpawnProcess {
                    proc: 4,
                    cmd: "definitely-not-a-real-binary-xyz".into(),
                    args: vec![],
                    cwd: None,
                },
            )
            .await
            .is_some_and(|e| matches!(e, Event::Error { .. })),
        "a failed spawn must report the error"
    );
    let exited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let ServerMessage::Notification {
                event: Event::ProcessExited { proc: 4, .. },
                ..
            } = rx.recv().await.expect("a server message")
            {
                break;
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "a failed spawn must emit ProcessExited");
}

/// When a child stops reading and its stdin queue fills, the server must not
/// silently drop frames (one lost chunk desyncs Content-Length framing
/// forever): it kills the process and says so.
#[tokio::test]
async fn stdin_overflow_kills_the_process_loudly() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(tx);
    let root = temp_project("stdin-overflow");
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        server
            .handle(
                2,
                Request::SpawnProcess {
                    proc: 5,
                    cmd: "sleep".into(),
                    args: vec!["30".into()],
                    cwd: None,
                },
            )
            .await
            .is_none()
    );
    // Pump until the queue (256 frames) plus the pipe are full; the overflow
    // must surface as an error, well before this generous cap.
    let chunk = vec![b'x'; 64 * 1024];
    let mut overflow = None;
    for i in 0..600u64 {
        if let Some(event) = server
            .handle(
                100 + i,
                Request::ProcessInput {
                    proc: 5,
                    data: chunk.clone(),
                },
            )
            .await
        {
            overflow = Some(event);
            break;
        }
    }
    match overflow {
        Some(Event::Error { message }) => {
            assert!(message.contains("overflow"), "unexpected error: {message}")
        }
        other => panic!("overflow must produce an error, got {other:?}"),
    }
    // …and the child is gone, without any ProcessKill from the client.
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let ServerMessage::Notification {
                event: Event::ProcessExited { proc: 5, .. },
                ..
            } = rx.recv().await.expect("a server message")
            {
                break;
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "an overflowed process must be killed");
}
