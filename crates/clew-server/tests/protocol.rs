//! End-to-end integration tests for the client↔server protocol.
//!
//! Most tests drive a `Server` in-process over a temp project through the
//! request flow, asserting the replies; one drives the real `clew-server`
//! binary over its stdio to check what only a whole process can show (the
//! shutdown when the client goes away).
//!
//! Hermetic by construction: every test runs against one throwaway data
//! directory (`CLEW_DATA_DIR`, set once before any server exists — never the
//! developer's real trust store or language-server store), every project and
//! scratch directory is unique to this run and removed when the test ends, git
//! runs without the global/system config, the fake model providers listen on
//! 127.0.0.1 only, every wait has a timeout, and no child outlives its test.

use clew_core::testutil::TempDir;
use clew_protocol::{
    AiChatConfig, AiChatMsg, ClientMessage, ErrorCode, Event, GitResult, PROTOCOL_VERSION,
    ProviderFailure, Refusal, Request, ServerMessage, StreamOutcome, TargetSpec,
};
use clew_server::{Server, SpawnPolicy};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

// -- harness -------------------------------------------------------------------

/// Longest any single wait in these tests may take before it fails (instead of
/// hanging the suite).
const WAIT: Duration = Duration::from_secs(30);

type Rx = mpsc::UnboundedReceiver<ServerMessage>;

/// The one data directory every test in this binary uses, created and exported
/// the first time any test asks — before that test starts a server, and never
/// changed afterwards — with git pointed away from the developer's own config. Servers read `CLEW_DATA_DIR` from arbitrary threads, so
/// a test that switched it mid-run would race the others' reads (and
/// `set_var` beside a concurrent `getenv` is undefined behaviour). No test
/// owns it, so it goes when the process exits.
fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let _env = clew_core::env_lock();
        let path = TempDir::new("server-it-data").until_exit();
        // SAFETY: serialized by env_lock, and it runs before any test of this
        // binary has started a server (every test's first step leads here).
        unsafe { std::env::set_var("CLEW_DATA_DIR", &path) };
        // And the servers' git sees the fixtures, not the developer's config.
        clew_core::testutil::isolate_git_config();
        path
    })
}

/// A throwaway project on disk with a couple of documented source files.
fn temp_project(name: &str) -> TempDir {
    data_dir();
    let dir = TempDir::new(name);
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

/// A fresh in-process server (a local client's) and its message stream.
fn server() -> (Server, Rx) {
    data_dir();
    let (tx, rx) = mpsc::unbounded_channel::<ServerMessage>();
    (Server::new(tx), rx)
}

fn host_target() -> TargetSpec {
    TargetSpec {
        label: "test".into(),
        os: "linux".into(),
        arch: "x86_64".into(),
        family: "unix".into(),
    }
}

/// Wait for the first message `pick` accepts, skipping the rest. Fails the
/// test after `limit` instead of hanging it.
async fn wait_for<T>(
    rx: &mut Rx,
    what: &str,
    limit: Duration,
    mut pick: impl FnMut(ServerMessage) -> Option<T>,
) -> T {
    let found = tokio::time::timeout(limit, async {
        loop {
            match rx.recv().await {
                Some(msg) => {
                    if let Some(found) = pick(msg) {
                        return found;
                    }
                }
                None => panic!("the server's stream ended while waiting for {what}"),
            }
        }
    })
    .await;
    found.unwrap_or_else(|_| panic!("no {what} within {limit:?}"))
}

/// Everything that arrives within `window` — for asserting that something
/// does NOT happen.
async fn collect_for(rx: &mut Rx, window: Duration) -> Vec<ServerMessage> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        seen.push(msg);
    }
    seen
}

/// Wait for the correlated `Reply` to request `id`, skipping notifications.
/// Slow arms answer from a detached task via the out channel, so the request
/// loop never stalls behind them.
async fn recv_reply(rx: &mut Rx, id: u64) -> Event {
    wait_for(
        rx,
        &format!("the reply to request {id}"),
        WAIT,
        |msg| match msg {
            ServerMessage::Reply { id: got, event, .. } if got == id => Some(event),
            _ => None,
        },
    )
    .await
}

/// Wait until the watcher is live and has delivered everything from before
/// it started. An in-place edit of `src/lib.rs` must come back as its
/// `FilesChanged` — the platform delivers file events in order, so one
/// replayed from a write made just before the watch began (the fixture's
/// own) has arrived by then — and what follows it (the edit's symbols) is
/// drained. A quiet window measured after this is about what the test does
/// next, not about the fixture.
async fn settle_watcher(rx: &mut Rx, root: &Path) {
    std::fs::write(root.join("src/lib.rs"), "pub fn settled_marker() {}\n").unwrap();
    recv_note(rx, "the settling edit", |event| match event {
        Event::FilesChanged { rels, .. } if rels.iter().any(|r| r == "src/lib.rs") => Some(()),
        _ => None,
    })
    .await;
    collect_for(rx, Duration::from_millis(800)).await;
}

/// A notification `pick` accepts.
async fn recv_note<T>(rx: &mut Rx, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    wait_for(rx, what, WAIT, |msg| match msg {
        ServerMessage::Notification { event, .. } => pick(event),
        _ => None,
    })
    .await
}

async fn hello(server: &mut Server) {
    let ready = server
        .handle(
            0,
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
            },
        )
        .await;
    assert!(matches!(ready, Some(Event::Ready { .. })), "{ready:?}");
}

/// Open a project and wait for its (asynchronous) Tree reply. Shakes hands
/// first — the server refuses every business request until a Hello with a
/// matching protocol version has completed (idempotent).
async fn open_project(server: &mut Server, rx: &mut Rx, id: u64, root: &Path) -> Vec<String> {
    hello(server).await;
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

/// Run git in `root`, isolated from the developer's global and system config
/// (a `commit.gpgsign` or a hook there must not decide these tests).
fn git(root: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?} failed");
}

/// Write an executable script.
fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// -- a fake model provider -------------------------------------------------------

/// How the fake provider answers.
#[derive(Clone, Copy)]
enum Pace {
    /// Stream an answer: SSE headers, then one token every `every`, up to
    /// `max` tokens — a provider generating (and billing) a long answer.
    Stream { every: Duration, max: usize },
    /// Accept the request and say nothing for up to this long.
    Silent(Duration),
    /// Refuse the request: answer with this status line and JSON body.
    Refuse(&'static str, &'static str),
}

/// A model provider on 127.0.0.1 (OpenAI-compatible), serving every
/// connection the same way. It reports when a request arrives, and when the
/// client hung up (for `Stream`: after how many tokens a write failed).
struct FakeProvider {
    base: String,
    requests: Arc<AtomicUsize>,
    hung_up: std::sync::mpsc::Receiver<usize>,
}

impl FakeProvider {
    fn new(pace: Pace) -> FakeProvider {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let (tx, hung_up) = std::sync::mpsc::channel();
        let counter = requests.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let tx = tx.clone();
                let counter = counter.clone();
                std::thread::spawn(move || {
                    clew_core::testutil::read_http_request(&mut conn);
                    counter.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.send(serve_connection(&mut conn, pace));
                });
            }
        });
        FakeProvider {
            base,
            requests,
            hung_up,
        }
    }

    /// Block until the first request has arrived.
    fn await_request(&self) {
        let began = Instant::now();
        while self.requests.load(Ordering::SeqCst) == 0 {
            assert!(began.elapsed() < WAIT, "the provider was never called");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn chat_config(&self) -> AiChatConfig {
        AiChatConfig {
            provider: "custom".into(),
            api_key: "k".into(),
            model: "m".into(),
            base_url: self.base.clone(),
        }
    }
}

/// Answer one connection per `pace`; returns how far it got before the
/// client hung up (tokens written for `Stream`, 0 for `Silent`).
fn serve_connection(conn: &mut std::net::TcpStream, pace: Pace) -> usize {
    match pace {
        Pace::Stream { every, max } => {
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            if conn.write_all(head.as_bytes()).is_err() {
                return 0;
            }
            for sent in 0..max {
                let event = "data: {\"choices\":[{\"delta\":{\"content\":\"token \"}}]}\n\n";
                // Once the client has closed, a write fails within a couple of
                // tokens (the first draws the reset, the next one errors).
                if conn
                    .write_all(event.as_bytes())
                    .and_then(|()| conn.flush())
                    .is_err()
                {
                    return sent;
                }
                std::thread::sleep(every);
            }
            let _ = conn.write_all(b"data: [DONE]\n\n");
            max
        }
        Pace::Silent(hold) => {
            // Reads return 0 as soon as the client closes its end.
            conn.set_read_timeout(Some(hold)).unwrap();
            let mut probe = [0u8; 1];
            let _ = conn.read(&mut probe);
            0
        }
        Pace::Refuse(status, body) => {
            let answer = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(answer.as_bytes());
            0
        }
    }
}

// -- the core flow -------------------------------------------------------------

#[tokio::test]
async fn protocol_round_trip() {
    let (mut server, mut rx) = server();
    let root = temp_project("roundtrip");

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
        Event::SearchResults {
            hits,
            error,
            skipped,
            skipped_total,
        } => {
            assert!(error.is_none(), "no search error: {error:?}");
            assert!(skipped.is_empty() && skipped_total == 0, "{skipped:?}");
            assert!(
                hits.iter().any(|h| h.rel == "src/lib.rs"),
                "found helper in lib.rs"
            );
        }
        other => panic!("expected SearchResults, got {other:?}"),
    }

    // BuildDocs runs on a detached task and replies from there — a reply
    // correlated by its request id, like every other request's.
    assert!(
        server.handle(5, Request::BuildDocs).await.is_none(),
        "BuildDocs replies async"
    );
    let files = match recv_reply(&mut rx, 5).await {
        Event::Docs { files, .. } => files,
        other => panic!("expected the Docs reply, got {other:?}"),
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
    let (mut server, mut rx) = server();
    let root = temp_project("confine");
    open_project(&mut server, &mut rx, 1, &root).await;

    // A path escaping the project must be refused, not read. The refusal is
    // synchronous — the path's shape is checked before any work is spawned.
    for bad in ["../../../../etc/passwd", "/etc/passwd", "src/../../x"] {
        let escaped = server
            .handle(
                2,
                Request::ReadFile {
                    rel: bad.into(),
                    target: host_target(),
                },
            )
            .await;
        assert!(
            matches!(escaped, Some(Event::Error { ref message, .. }) if message.contains("escapes")),
            "{bad} must be refused, got {escaped:?}"
        );
    }
}

/// Confinement holds through SYMLINKS for every request that takes a path:
/// a link inside the project that points out of it is refused by `ReadFile`
/// and `GitInfo`, and left out of a `ReadSources` batch — which feeds a model
/// prompt, so a leak there would send another file to the provider.
#[cfg(unix)]
#[tokio::test]
async fn confinement_holds_through_symlinks_for_every_path_request() {
    let (mut server, mut rx) = server();
    let root = temp_project("confine-links");
    let outside = TempDir::new("confine-outside");
    std::fs::write(outside.join("secret.txt"), "SECRET").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(&*outside, root.join("src/vendor")).unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;

    for (id, rel) in [(2, "leak.txt"), (3, "src/vendor/secret.txt")] {
        assert!(
            server
                .handle(
                    id,
                    Request::ReadFile {
                        rel: rel.into(),
                        target: host_target(),
                    },
                )
                .await
                .is_none()
        );
        match recv_reply(&mut rx, id).await {
            Event::Error { message, .. } => {
                assert!(message.contains("escapes"), "{rel}: {message}")
            }
            other => panic!("{rel} must be refused, got {other:?}"),
        }
    }
    assert!(
        server
            .handle(
                4,
                Request::GitInfo {
                    rel: "leak.txt".into()
                }
            )
            .await
            .is_none()
    );
    // Refused for escaping — not merely failing (a non-repository answers
    // `GitInfo { info: None }`, never an error, so only the confinement can
    // produce this one).
    match recv_reply(&mut rx, 4).await {
        Event::Error { code, message } => {
            assert_eq!(code, clew_protocol::ErrorCode::Refused, "{message}");
            assert!(message.contains("escapes"), "{message}");
        }
        other => panic!("a GitInfo through a link out must be refused, got {other:?}"),
    }

    assert!(
        server
            .handle(
                5,
                Request::ReadSources {
                    rels: vec![
                        "leak.txt".into(),
                        "src/vendor/secret.txt".into(),
                        "src/lib.rs".into(),
                    ],
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 5).await {
        Event::Sources { files, .. } => {
            let rels: Vec<&str> = files.iter().map(|(rel, _)| rel.as_str()).collect();
            assert_eq!(rels, ["src/lib.rs"], "only the confined file");
            assert!(files.iter().all(|(_, text)| !text.contains("SECRET")));
        }
        other => panic!("expected Sources, got {other:?}"),
    }

    // A file that is simply missing is an error, not a claimed escape.
    assert!(
        server
            .handle(
                6,
                Request::ReadFile {
                    rel: "src/missing.rs".into(),
                    target: host_target(),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 6).await {
        Event::Error { message, .. } => {
            assert!(
                message.starts_with("src/missing.rs:") && !message.contains("escapes"),
                "{message}"
            )
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

/// A `Sources` reply names the rels that do not exist apart from those that
/// could not be read — with the error — and those too large to explain or
/// not plain text files apart from both. The client's Explain pass drops
/// what it recorded for a missing, too large or refused file and keeps it
/// for one it could not read: a file over the cap, one this user may not
/// read, a dangling link or a link out of the project was left out exactly
/// as a deleted file was, and then all of them were held as ones that could
/// not be read.
#[cfg(unix)]
#[tokio::test]
async fn a_sources_reply_tells_a_missing_file_from_an_unreadable_one() {
    use std::os::unix::fs::PermissionsExt;
    let (mut server, mut rx) = server();
    let root = temp_project("sources-missing");
    let outside = TempDir::new("sources-missing-outside");
    std::fs::write(outside.join("secret.rs"), "fn secret() {}\n").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.rs"), root.join("src/leak.rs")).unwrap();
    std::os::unix::fs::symlink(root.join("src/nowhere.rs"), root.join("src/dangling.rs")).unwrap();
    let cap = clew_core::fs_scan::MAX_INDEX_FILE_BYTES as usize;
    std::fs::write(root.join("src/big.rs"), "x".repeat(cap + 1)).unwrap();
    std::fs::write(root.join("src/latin1.rs"), b"// caf\xe9\n").unwrap();
    std::fs::write(root.join("src/locked.rs"), "fn locked() {}\n").unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;

    let locked = root.join("src/locked.rs");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        // Root reads everything: there is no unreadable file to observe.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }
    let rels = [
        "src/lib.rs",
        "src/gone.rs",
        "src/big.rs",
        "src/latin1.rs",
        "src/locked.rs",
        "src/dangling.rs",
        "src/leak.rs",
        "src/no/such/dir.rs",
    ];
    let request = Request::ReadSources {
        rels: rels.iter().map(|r| r.to_string()).collect(),
    };
    assert!(server.handle(2, request).await.is_none());
    let reply = recv_reply(&mut rx, 2).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    match reply {
        Event::Sources {
            files,
            missing,
            too_large,
            refused,
            unreadable,
            deferred,
            ..
        } => {
            let read: Vec<&str> = files.iter().map(|(rel, _)| rel.as_str()).collect();
            assert_eq!(read, ["src/lib.rs"]);
            assert_eq!(missing, ["src/gone.rs", "src/no/such/dir.rs"]);
            assert_eq!(too_large, [("src/big.rs".to_string(), cap as u64 + 1)]);
            // The locked file could not be read, and is said so, with the
            // error. Each refused one is said to be what it is.
            let [(rel, why)] = &unreadable[..] else {
                panic!("the locked file alone is unreadable: {unreadable:?}");
            };
            assert_eq!(rel, "src/locked.rs");
            assert!(why.contains("ermission denied"), "{why}");
            let refused: Vec<(&str, Refusal)> = refused
                .iter()
                .map(|(rel, why)| (rel.as_str(), *why))
                .collect();
            assert_eq!(
                refused,
                [
                    ("src/latin1.rs", Refusal::NotUtf8),
                    ("src/dangling.rs", Refusal::NotPlainFile),
                    ("src/leak.rs", Refusal::OutsideProject),
                ]
            );
            assert!(deferred.is_empty(), "{deferred:?}");
        }
        other => panic!("expected Sources, got {other:?}"),
    }
}

/// After the scan commits, the server pushes the full project-symbol
/// snapshot — the data a remote client's index is built from, extracted
/// where the files actually live.
#[tokio::test]
async fn open_project_pushes_a_symbol_snapshot() {
    let (mut server, mut rx) = server();
    let root = temp_project("symbol-snapshot");
    // An impl block, so the structure index has something to say.
    std::fs::write(
        root.join("src/shape.rs"),
        "pub struct Circle;\nimpl Circle { pub fn area(&self) -> f64 { 1.0 } }\n",
    )
    .unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;

    let (snapshot, structure) =
        recv_note(
            &mut rx,
            "a full ProjectSymbols snapshot",
            |event| match event {
                Event::ProjectSymbols {
                    root: snap_root,
                    full: true,
                    files,
                    structure,
                    ..
                } => {
                    assert_eq!(snap_root, root.to_string_lossy());
                    Some((files, structure))
                }
                _ => None,
            },
        )
        .await;
    let lib = snapshot
        .iter()
        .find(|f| f.rel == "src/lib.rs")
        .expect("lib.rs indexed");
    assert!(
        lib.symbols.iter().any(|s| s.name == "add"),
        "symbols extracted where the files live: {:?}",
        lib.symbols
    );
    // The type/trait structure rides along (the fixture's impl block).
    let clew_protocol::Patch::Set(Some(structure)) = structure else {
        panic!("a full snapshot always recomputes the structure index");
    };
    let line = structure.summary_line("Circle").expect("Circle indexed");
    assert!(line.contains("1 method"), "{line}");
}

/// The `Git` bridge runs where the repository lives, and refuses arguments
/// that could steer the git invocation (escaping paths, non-hex "shas",
/// option-shaped refs).
#[tokio::test]
async fn git_ops_run_where_the_repo_lives_and_validate_args() {
    let (mut server, mut rx) = server();
    let root = temp_project("git-bridge");
    // A real repo with one commit, so history has something to say.
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    open_project(&mut server, &mut rx, 1, &root).await;

    assert!(
        server
            .handle(
                2,
                Request::Git {
                    op: clew_protocol::GitOp::FileHistory {
                        rel: "src/lib.rs".into(),
                        limit: 10,
                    },
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 2).await {
        Event::GitResult {
            root: git_root,
            result,
        } => {
            assert_eq!(git_root, root.to_string_lossy());
            let GitResult::FileHistory(commits) = result else {
                panic!("a FileHistory op is answered with FileHistory, got {result:?}");
            };
            assert_eq!(commits.len(), 1, "one commit: {commits:?}");
            assert_eq!(commits[0].subject, "initial");
        }
        other => panic!("expected GitResult, got {other:?}"),
    }

    // Hostile arguments are refused before any subprocess.
    let bad_ops = [
        clew_protocol::GitOp::FileHistory {
            rel: "../outside.rs".into(),
            limit: 10,
        },
        clew_protocol::GitOp::CommitMessage {
            sha: "--help".into(),
        },
        clew_protocol::GitOp::RangePatch {
            base: "--exec=evil".into(),
            max_bytes: 100,
        },
    ];
    for op in bad_ops {
        let refused = server.handle(3, Request::Git { op: op.clone() }).await;
        assert!(
            matches!(
                refused,
                Some(Event::Error {
                    code: ErrorCode::Refused,
                    ..
                })
            ),
            "{op:?} must be refused, got {refused:?}"
        );
    }
}

/// A git that could not answer is answered as an ERROR, never as an empty
/// result: `GitInfo` and `Git` sent into a real git failure — a repository
/// whose blob is gone, and a well-formed commit id that names nothing — come
/// back as `Event::Error` (`Failed`) with git's reason. Only successes and
/// refusals were tested at this level.
#[tokio::test]
async fn a_git_failure_is_answered_as_an_error_not_an_empty_result() {
    let (mut server, mut rx) = server();
    let root = temp_project("git-failure");
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    open_project(&mut server, &mut rx, 1, &root).await;
    let rev = |spec: &str| {
        let out = std::process::Command::new("git")
            .args(["rev-parse", spec])
            .current_dir(&*root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "rev-parse {spec}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let head = rev("HEAD");
    let blob = rev("HEAD:src/lib.rs");
    // The object HEAD's `src/lib.rs` points at is gone, and the file is
    // edited, so blaming or diffing it has to read that object: git can no
    // longer answer.
    std::fs::remove_file(root.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
    let edited = std::fs::read_to_string(root.join("src/lib.rs")).unwrap() + "// edited\n";
    std::fs::write(root.join("src/lib.rs"), edited).unwrap();
    let failed = |event: Event| match event {
        Event::Error {
            code: ErrorCode::Failed,
            message,
        } => message,
        other => panic!("expected a Failed error, got {other:?}"),
    };

    let info = Request::GitInfo {
        rel: "src/lib.rs".into(),
    };
    assert!(server.handle(2, info).await.is_none());
    let message = failed(recv_reply(&mut rx, 2).await);
    assert!(message.contains("git blame of src/lib.rs"), "{message}");

    let file_at = clew_protocol::GitOp::FileAt {
        sha: head,
        rel: "src/lib.rs".into(),
    };
    assert!(
        server
            .handle(3, Request::Git { op: file_at })
            .await
            .is_none()
    );
    failed(recv_reply(&mut rx, 3).await);

    let nothing = clew_protocol::GitOp::CommitMessage {
        sha: "0123456789abcdef0123456789abcdef01234567".into(),
    };
    assert!(
        server
            .handle(4, Request::Git { op: nothing })
            .await
            .is_none()
    );
    failed(recv_reply(&mut rx, 4).await);
}

/// `Stats` computes where the project lives and replies with the report — a
/// remote client never walks its own disk for it.
#[tokio::test]
async fn stats_compute_where_the_project_lives() {
    let (mut server, mut rx) = server();
    let root = temp_project("stats");
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(server.handle(2, Request::Stats).await.is_none());
    match recv_reply(&mut rx, 2).await {
        Event::Stats {
            root: stats_root,
            report,
        } => {
            assert_eq!(stats_root, root.to_string_lossy());
            assert!(
                report.langs.iter().any(|l| l.name == "Rust"),
                "Rust counted: {report:?}"
            );
        }
        other => panic!("expected Stats, got {other:?}"),
    }
}

/// `ProjectCalls` builds the name-based call graph where the files live,
/// with project-relative node paths on the wire.
#[tokio::test]
async fn project_calls_build_where_the_project_lives() {
    let (mut server, mut rx) = server();
    let root = temp_project("project-calls");
    std::fs::write(
        root.join("src/calls.rs"),
        "fn callee() {}\nfn caller() { callee(); }\n",
    )
    .unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        server
            .handle(2, Request::ProjectCalls { scope: Vec::new() })
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 2).await {
        Event::ProjectCalls {
            root: graph_root,
            graph,
        } => {
            assert_eq!(graph_root, root.to_string_lossy());
            let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
            assert!(
                names.contains(&"callee") && names.contains(&"caller"),
                "both functions in the graph: {names:?}"
            );
            // Paths are project-relative on the wire.
            assert!(
                graph
                    .nodes
                    .iter()
                    .all(|n| n.file == "src/calls.rs"
                        || !n.file.starts_with(&*root.to_string_lossy())),
                "wire paths must be rels: {graph:?}"
            );
            assert!(graph.nodes.iter().any(|n| n.file == "src/calls.rs"));
            // …and they convert into the client's graph, validated.
            let graph =
                clew_core::projectcalls::ProjectCallGraph::from_wire(graph, |rel| root.join(rel))
                    .expect("a server-built graph validates");
            assert_eq!(graph.edge_count(), 1);
        }
        other => panic!("expected ProjectCalls, got {other:?}"),
    }
}

/// `SpawnAdapter` for a TCP-transport language must refuse with a reason AND
/// end the proxied stream (ProcessExited), so the client's DAP driver sees
/// EOF instead of waiting on an adapter that will never exist.
#[tokio::test]
async fn spawn_adapter_refuses_tcp_langs_and_ends_the_stream() {
    let (mut server, mut rx) = server();
    let root = temp_project("adapter-tcp");
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        server
            .handle(
                2,
                Request::SpawnAdapter {
                    proc: 9,
                    lang: "go".into(),
                    program: root.join("main").to_string_lossy().into_owned(),
                    args: vec![],
                },
            )
            .await
            .is_none()
    );
    let (mut exited, mut refused) = (false, false);
    while !(exited && refused) {
        wait_for(&mut rx, "the refusal and the exit", WAIT, |msg| match msg {
            ServerMessage::Notification {
                event: Event::ProcessExited { proc: 9, .. },
                ..
            } => {
                exited = true;
                Some(())
            }
            ServerMessage::Reply {
                id: 2,
                event: Event::Error { message, .. },
                ..
            } => {
                assert!(message.contains("TCP"), "{message}");
                refused = true;
                Some(())
            }
            _ => None,
        })
        .await;
    }
}

// -- project state -------------------------------------------------------------

/// Project state (`<root>/.clew/*`) reads and writes happen where the
/// project lives, under the statefile rules — and a rel that escapes
/// `.clew/` is refused outright.
#[tokio::test]
async fn state_files_read_and_write_where_the_project_lives() {
    let (mut server, mut rx) = server();
    let root = temp_project("state-rw");
    open_project(&mut server, &mut rx, 1, &root).await;
    let root_str = root.to_string_lossy().into_owned();

    // Write, then read back.
    let json = r#"[{"rel":"src/lib.rs","line":2,"preview":"pub fn add"}]"#;
    assert!(
        server
            .handle(
                2,
                Request::WriteState {
                    root: root_str.clone(),
                    rel: "bookmarks.json".into(),
                    text: Some(json.into()),
                },
            )
            .await
            .is_none(),
        "the write is acknowledged asynchronously"
    );
    // Success is acknowledged, not silent: a queued frame is not a durable
    // write, so the client has to be told the bytes reached the disk before it
    // may drop its own copy of the change.
    match recv_reply(&mut rx, 2).await {
        Event::StateWritten {
            root: ack_root,
            rel,
        } => {
            assert_eq!(ack_root, root_str);
            assert_eq!(rel, "bookmarks.json");
        }
        other => panic!("expected StateWritten, got {other:?}"),
    }
    assert!(
        server
            .handle(
                3,
                Request::ReadState {
                    root: root_str.clone(),
                    rel: "bookmarks.json".into(),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 3).await {
        Event::StateContent {
            root: state_root,
            rel,
            text,
        } => {
            assert_eq!(state_root, root_str);
            assert_eq!(rel, "bookmarks.json");
            assert_eq!(text.as_deref(), Some(json));
        }
        other => panic!("expected StateContent, got {other:?}"),
    }

    // `text: None` deletes; the next read reports it missing.
    assert!(
        server
            .handle(
                4,
                Request::WriteState {
                    root: root_str.clone(),
                    rel: "bookmarks.json".into(),
                    text: None,
                },
            )
            .await
            .is_none()
    );
    assert!(
        server
            .handle(
                5,
                Request::ReadState {
                    root: root_str.clone(),
                    rel: "bookmarks.json".into(),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 5).await {
        Event::StateContent { text: None, .. } => {}
        other => panic!("deleted state must read as missing, got {other:?}"),
    }

    // A rel that escapes .clew/ is refused before any filesystem access.
    for bad in ["../evil.json", "/etc/passwd", "a/../../b"] {
        let refused = server
            .handle(
                6,
                Request::ReadState {
                    root: root_str.clone(),
                    rel: bad.into(),
                },
            )
            .await;
        assert!(
            matches!(refused, Some(Event::Error { .. })),
            "escaping rel {bad:?} must be refused, got {refused:?}"
        );
        let refused = server
            .handle(
                7,
                Request::WriteState {
                    root: root_str.clone(),
                    rel: bad.into(),
                    text: Some("x".into()),
                },
            )
            .await;
        assert!(
            matches!(refused, Some(Event::Error { .. })),
            "escaping rel {bad:?} must be refused, got {refused:?}"
        );
    }
}

/// fixR1 #13, end to end: an `EditState` is applied once per `edit_id`. The
/// client replays an edit whose reply died with its transport — over a new
/// transport, to a new server process, since the old one went with its SSH
/// session — and the toggle it carries must not be undone by the replay. The
/// replay is answered like the edit was: with the file as it now is.
///
/// The "new server" is a second in-process `Server`, not a second process.
/// That stands in for one because nothing about applied edits is kept in
/// memory: `statefile::merge_file` reads and writes its ledger on disk, under
/// the store's lock, on every edit.
#[tokio::test]
async fn a_replayed_state_edit_is_not_applied_twice_by_a_new_server() {
    let root = temp_project("state-replay");
    let root_str = root.to_string_lossy().into_owned();
    let toggle = |edit_id: &str| Request::EditState {
        root: root_str.clone(),
        rel: "bookmarks.json".into(),
        merge: clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "line".into()],
            key: vec!["b.rs".into(), 2.into()],
            edit: clew_protocol::StateEdit::Toggle(serde_json::json!({"rel": "b.rs", "line": 2})),
            delete_when_empty: true,
        },
        edit_id: edit_id.into(),
    };
    let bookmarked = || {
        std::fs::read_to_string(root.join(".clew/bookmarks.json"))
            .is_ok_and(|text| text.contains("b.rs"))
    };

    // The first server applies it; its reply is what the dead transport lost.
    let (mut first, mut rx) = server();
    open_project(&mut first, &mut rx, 1, &root).await;
    assert!(first.handle(2, toggle("w9-1")).await.is_none());
    let Event::StateEdited { text: landed, .. } = recv_reply(&mut rx, 2).await else {
        panic!("the edit must be applied");
    };
    assert!(bookmarked());
    drop(first);

    // The reconnect reaches a new server, and the edit is sent again.
    let (mut second, mut rx) = server();
    open_project(&mut second, &mut rx, 1, &root).await;
    assert!(second.handle(2, toggle("w9-1")).await.is_none());
    match recv_reply(&mut rx, 2).await {
        Event::StateEdited { text, .. } => assert_eq!(text, landed, "answered with the file"),
        other => panic!("a replay must be answered like its edit, got {other:?}"),
    }
    assert!(bookmarked(), "the replayed toggle undid the bookmark");

    // A new press is a new id: it applies. An id that is not one is refused.
    assert!(second.handle(3, toggle("w9-2")).await.is_none());
    assert!(matches!(
        recv_reply(&mut rx, 3).await,
        Event::StateEdited { .. }
    ));
    assert!(!bookmarked());
    match second.handle(4, toggle("../../x")).await {
        Some(Event::Error { code, message }) => {
            assert_eq!(code, clew_protocol::ErrorCode::Refused, "{message}");
            assert!(message.contains("bad edit id"), "{message}");
        }
        other => panic!("a malformed edit id must be refused, got {other:?}"),
    }
}

/// A state file that exists but cannot be read safely, or that this build
/// cannot parse, is reported and left exactly as it is — never read as
/// "missing" and replaced. A single bookmark toggle used to rewrite a store it
/// could not parse as a one-entry file.
#[tokio::test]
async fn a_state_file_that_cannot_be_read_or_parsed_is_never_overwritten() {
    let (mut server, mut rx) = server();
    let root = temp_project("state-refused");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    let garbage = "[{\"rel\":\"a.rs\",\"line\":1},\n<<<<<<< HEAD\n";
    std::fs::write(root.join(".clew/bookmarks.json"), garbage).unwrap();
    let broken_history = "{\"schema_version\":1,\"nodes\":[ oops";
    std::fs::write(root.join(".clew/history.json"), broken_history).unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;
    let root_str = root.to_string_lossy().into_owned();

    // An entry-level merge into the unparseable store: refused, untouched.
    assert!(
        server
            .handle(
                2,
                Request::EditState {
                    root: root_str.clone(),
                    rel: "bookmarks.json".into(),
                    merge: clew_protocol::StateMerge {
                        key_fields: vec!["rel".into(), "line".into()],
                        key: vec!["b.rs".into(), 2.into()],
                        edit: clew_protocol::StateEdit::Toggle(
                            serde_json::json!({"rel": "b.rs", "line": 2}),
                        ),
                        delete_when_empty: true,
                    },
                    edit_id: "c1-1".into(),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 2).await {
        Event::Error { message, .. } => assert!(message.contains("bookmarks.json"), "{message}"),
        other => panic!("the merge must be refused, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(root.join(".clew/bookmarks.json")).unwrap(),
        garbage
    );

    // A wholesale write over a history this build cannot parse: refused.
    assert!(
        server
            .handle(
                3,
                Request::WriteState {
                    root: root_str.clone(),
                    rel: "history.json".into(),
                    text: Some(r#"{"schema_version":1,"nodes":[],"current":null}"#.into()),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 3).await {
        Event::Error { message, .. } => assert!(message.contains("not valid JSON"), "{message}"),
        other => panic!("the write must be refused, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(root.join(".clew/history.json")).unwrap(),
        broken_history
    );

    // A read the statefile rules refuse answers with an error, not "missing":
    // "missing" is what releases the client's held writes over the file.
    #[cfg(unix)]
    {
        let outside = TempDir::new("state-outside");
        std::fs::write(outside.join("notes.json"), "[]").unwrap();
        std::os::unix::fs::symlink(outside.join("notes.json"), root.join(".clew/notes.json"))
            .unwrap();
        assert!(
            server
                .handle(
                    4,
                    Request::ReadState {
                        root: root_str.clone(),
                        rel: "notes.json".into(),
                    },
                )
                .await
                .is_none()
        );
        match recv_reply(&mut rx, 4).await {
            Event::Error { message, .. } => assert!(message.contains("notes.json"), "{message}"),
            other => panic!("a refused read must not read as missing, got {other:?}"),
        }
    }
}

/// A state write names the project it belongs to, and the server refuses one
/// for any other. Without that, a save racing a project switch resolved
/// against whatever root the server happened to hold — writing one project's
/// bookmarks over another's, since these writes replace the file wholesale.
#[tokio::test]
async fn a_state_write_for_another_project_is_refused() {
    let (mut server, mut rx) = server();
    let root = temp_project("state-root");
    open_project(&mut server, &mut rx, 1, &root).await;

    let refused = server
        .handle(
            2,
            Request::WriteState {
                root: "/some/other/project".into(),
                rel: "bookmarks.json".into(),
                text: Some("[]".into()),
            },
        )
        .await;
    match refused {
        Some(Event::Error { message, .. }) => {
            assert!(message.contains("refused: state"), "{message}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(
        !root.join(".clew/bookmarks.json").exists(),
        "the refused write must not have touched this project"
    );

    // The same write naming THIS project is applied.
    assert!(
        server
            .handle(
                3,
                Request::WriteState {
                    root: root.to_string_lossy().into_owned(),
                    rel: "bookmarks.json".into(),
                    text: Some("[]".into()),
                },
            )
            .await
            .is_none()
    );
    // The state worker is ordered, so a read behind it sees the write.
    assert!(
        server
            .handle(
                4,
                Request::ReadState {
                    root: root.to_string_lossy().into_owned(),
                    rel: "bookmarks.json".into(),
                },
            )
            .await
            .is_none()
    );
    match recv_reply(&mut rx, 4).await {
        Event::StateContent { text, .. } => assert_eq!(text.as_deref(), Some("[]")),
        other => panic!("expected StateContent, got {other:?}"),
    }
}

// -- handshake and routing -----------------------------------------------------

/// Business requests before ANY Hello are refused — the handshake is fail
/// closed on both ends, not just after a version mismatch.
#[tokio::test]
async fn requests_before_hello_are_refused() {
    let (mut server, _rx) = server();
    let refused = server.handle(1, Request::ListDir { path: None }).await;
    assert!(
        matches!(refused, Some(Event::Error { ref message, .. }) if message.contains("handshake")),
        "pre-handshake requests must be refused, got {refused:?}"
    );
}

/// `ListDir` answers from a blocking task (never on the request loop, which a
/// hung mount would otherwise stall), correlated by id.
#[tokio::test]
async fn list_dir_lists_the_host() {
    let (mut server, mut rx) = server();
    let root = temp_project("listdir");
    hello(&mut server).await;
    assert!(
        server
            .handle(
                1,
                Request::ListDir {
                    path: Some(root.to_string_lossy().into_owned()),
                },
            )
            .await
            .is_none(),
        "ListDir replies async"
    );
    match recv_reply(&mut rx, 1).await {
        Event::DirListing { entries, .. } => {
            assert!(
                entries.iter().any(|e| e.name == "src" && e.is_dir),
                "src dir listed as a directory"
            );
        }
        other => panic!("expected DirListing, got {other:?}"),
    }
}

/// A version mismatch is refused at the handshake with an explanatory error.
/// Silently proceeding used to fail far later and far more confusingly: the
/// peer's frames simply didn't deserialize and vanished.
#[tokio::test]
async fn hello_refuses_a_protocol_mismatch() {
    let (mut server, _rx) = server();
    let refused = server
        .handle(
            1,
            Request::Hello {
                protocol: PROTOCOL_VERSION - 1,
                fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
            },
        )
        .await;
    match refused {
        Some(Event::Error {
            code: ErrorCode::Handshake,
            message,
        }) => {
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
        matches!(pipelined, Some(Event::Error { code: ErrorCode::Handshake, ref message })
            if message.contains("handshake")),
        "requests after a failed handshake must be refused, got {pipelined:?}"
    );
    // The matching version still shakes hands (and reopens the connection).
    hello(&mut server).await;
}

/// Same numeric version but a different protocol BUILD is refused too: the
/// fingerprint is what catches a wire change whose version bump was missed
/// (or a stale dev binary) before it becomes silent frame drops.
#[tokio::test]
async fn hello_refuses_a_fingerprint_mismatch() {
    let (mut server, _rx) = server();
    let refused = server
        .handle(
            1,
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                fingerprint: "0000000000000000".into(),
            },
        )
        .await;
    match refused {
        Some(Event::Error {
            code: ErrorCode::Handshake,
            message,
        }) => {
            assert!(message.contains("protocol build mismatch"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let pipelined = server
        .handle(
            2,
            Request::OpenProject {
                root: "/tmp".into(),
            },
        )
        .await;
    assert!(
        matches!(pipelined, Some(Event::Error { code: ErrorCode::Handshake, ref message })
            if message.contains("handshake")),
        "requests after a failed handshake must be refused, got {pipelined:?}"
    );
}

/// A request pipelined behind `OpenProject` lands in the scan window, before
/// the file list has committed. Those requests used to fall through `?` into
/// silence — no reply at all — so the client's spinner ran forever. They must
/// now either wait for the scan or answer with the retryable refusal.
#[tokio::test]
async fn a_request_pipelined_behind_open_project_is_always_answered() {
    let (mut server, mut rx) = server();
    let root = temp_project("scan-window");
    hello(&mut server).await;
    // Do NOT await the Tree: this search is issued while the scan is still
    // running, which is exactly the window that used to swallow it.
    assert!(
        server
            .handle(
                1,
                Request::OpenProject {
                    root: root.to_string_lossy().into_owned(),
                },
            )
            .await
            .is_none()
    );
    assert!(
        server
            .handle(
                2,
                Request::Search {
                    query: "origin".into(),
                    regex: false,
                    case_sensitive: false,
                    whole_word: false,
                    include: String::new(),
                    exclude: String::new(),
                },
            )
            .await
            .is_none(),
        "Search replies asynchronously"
    );
    match recv_reply(&mut rx, 2).await {
        // The scan committed in time: real results.
        Event::SearchResults { .. } => {}
        // Or the bounded wait expired — but it is an ANSWER, and one the
        // client is allowed to retry (it retries on the code, not the text).
        Event::Error {
            code: ErrorCode::NotReady,
            ..
        } => {}
        other => panic!("the request must be answered, got {other:?}"),
    }
}

// -- the watcher ---------------------------------------------------------------

/// The watcher refreshes the server's shared file list after a structural
/// change, so search greps the current file set — not the one from the last
/// `OpenProject`. (The root is left un-canonicalized on purpose: the system
/// temp dir is a symlink on macOS, and events must still map back to it.)
#[tokio::test]
async fn search_sees_files_created_after_open() {
    let (mut server, mut rx) = server();
    let root = temp_project("watch-files");
    open_project(&mut server, &mut rx, 1, &root).await;

    // A file appears after the open (as if created by a build or an editor).
    std::fs::write(root.join("src/fresh.rs"), "fn brand_new_needle() {}\n").unwrap();

    // Wait for the watcher's rescan (debounced) to land: the Tree notification
    // is sent after the shared file list has been refreshed.
    recv_note(&mut rx, "a Tree with the new file", |event| match event {
        Event::Tree { files, .. } if files.iter().any(|f| f == "src/fresh.rs") => Some(()),
        _ => None,
    })
    .await;

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

/// Every `OpenProject` is answered exactly once, however opens race each
/// other: the one that stands with its `Tree`, each one a newer open
/// superseded with `Cancelled` — never with silence, which left a client
/// waiting on a reply that could not come. Rapid opens of two projects make
/// the supersession race real without depending on which scan wins it.
#[tokio::test]
async fn every_open_project_is_answered_exactly_once() {
    let (mut server, mut rx) = server();
    let a = temp_project("open-race-a");
    let b = temp_project("open-race-b");
    hello(&mut server).await;
    let opens: Vec<u64> = (1..=6).collect();
    for &id in &opens {
        let root = if id % 2 == 1 { &a } else { &b };
        assert!(
            server
                .handle(
                    id,
                    Request::OpenProject {
                        root: root.to_string_lossy().into_owned(),
                    },
                )
                .await
                .is_none()
        );
    }
    let mut answers: std::collections::BTreeMap<u64, Vec<Event>> = Default::default();
    while answers.len() < opens.len() {
        let (id, event) = wait_for(&mut rx, "an OpenProject reply", WAIT, |msg| match msg {
            ServerMessage::Reply { id, event } => Some((id, event)),
            ServerMessage::Notification { .. } => None,
        })
        .await;
        answers.entry(id).or_default().push(event);
    }
    // Nothing else trails in for any of them.
    for msg in collect_for(&mut rx, Duration::from_millis(500)).await {
        if let ServerMessage::Reply { id, event } = msg {
            answers.entry(id).or_default().push(event);
        }
    }
    for (id, events) in &answers {
        assert_eq!(
            events.len(),
            1,
            "open {id} answered {} times: {events:?}",
            events.len()
        );
        assert!(
            matches!(
                events[0],
                Event::Tree { .. }
                    | Event::Error {
                        code: ErrorCode::Cancelled,
                        ..
                    }
            ),
            "open {id}: {:?}",
            events[0]
        );
    }
    // The last open stands: nothing newer could supersede it.
    let last = opens[opens.len() - 1];
    match &answers[&last][0] {
        Event::Tree { root, .. } => assert_eq!(root, &b.to_string_lossy()),
        other => panic!("the newest open must be the one that stands, got {other:?}"),
    }
}

/// Every tree is stamped in scan order from one server-lifetime counter: a
/// watcher rescan outranks the open's own tree, and a later open outranks
/// both — which is what lets the client drop a tree that saw less than the
/// one on screen.
#[tokio::test]
async fn trees_are_stamped_in_scan_order() {
    let (mut server, mut rx) = server();
    let root = temp_project("tree-seq");
    hello(&mut server).await;
    let open = || Request::OpenProject {
        root: root.to_string_lossy().into_owned(),
    };
    assert!(server.handle(1, open()).await.is_none());
    let opened = match recv_reply(&mut rx, 1).await {
        Event::Tree { seq, .. } => seq,
        other => panic!("expected Tree, got {other:?}"),
    };
    assert!(opened >= 1, "the first scan is 1 or later: {opened}");

    std::fs::write(root.join("src/fresh.rs"), "fn fresh() {}\n").unwrap();
    let rescanned = recv_note(&mut rx, "the rescan's tree", |event| match event {
        Event::Tree { seq, files, .. } if files.iter().any(|f| f == "src/fresh.rs") => Some(seq),
        _ => None,
    })
    .await;
    assert!(rescanned > opened, "{rescanned} must outrank {opened}");

    assert!(server.handle(2, open()).await.is_none());
    let reopened = match recv_reply(&mut rx, 2).await {
        Event::Tree { seq, .. } => seq,
        other => panic!("expected Tree, got {other:?}"),
    };
    assert!(reopened > rescanned, "{reopened} must outrank {rescanned}");
}

/// VCS churn, build output and the server's OWN `.clew/` writes are noise:
/// they used to force a full rescan and a whole-tree push on every debounce,
/// because `structural` was decided from the event kind before the noise
/// filter ran. They must push nothing, while a real change still does.
#[tokio::test]
async fn the_watcher_ignores_noise_including_its_own_state_writes() {
    let (mut server, mut rx) = server();
    let project = temp_project("watch-noise");
    for dir in [".git/objects", "target/debug"] {
        std::fs::create_dir_all(project.join(dir)).unwrap();
    }
    let root = std::fs::canonicalize(&*project).unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;
    // The open's own publication lands first, so it cannot be mistaken for
    // noise below.
    recv_note(&mut rx, "the open's symbol snapshot", |event| match event {
        Event::ProjectSymbols { full: true, .. } => Some(()),
        _ => None,
    })
    .await;
    settle_watcher(&mut rx, &root).await;

    // The server's own atomic write into `.clew/` (temp file + rename)...
    assert!(
        server
            .handle(
                2,
                Request::WriteState {
                    root: root.to_string_lossy().into_owned(),
                    rel: "bookmarks.json".into(),
                    text: Some("[]".into()),
                },
            )
            .await
            .is_none()
    );
    assert!(matches!(
        recv_reply(&mut rx, 2).await,
        Event::StateWritten { .. }
    ));
    // ...git's lock churn and a build dropping artifacts.
    std::fs::write(root.join(".git/index.lock"), "x").unwrap();
    std::fs::remove_file(root.join(".git/index.lock")).unwrap();
    std::fs::write(root.join(".git/objects/ab12"), "x").unwrap();
    std::fs::write(root.join("target/debug/app.o"), "x").unwrap();

    let noise = collect_for(&mut rx, Duration::from_secs(2)).await;
    for msg in &noise {
        if let ServerMessage::Notification { event, .. } = msg {
            assert!(
                !matches!(
                    event,
                    Event::Tree { .. } | Event::FilesChanged { .. } | Event::ProjectSymbols { .. }
                ),
                "noise must publish nothing, got {event:?}"
            );
        }
    }

    // The watcher is live: a real file still rescans.
    std::fs::write(root.join("src/real.rs"), "fn real() {}\n").unwrap();
    recv_note(&mut rx, "a Tree with the real file", |event| match event {
        Event::Tree { files, .. } if files.iter().any(|f| f == "src/real.rs") => Some(()),
        _ => None,
    })
    .await;
}

/// A symlinked directory is not part of the project (the scanner does not
/// follow it), so the watcher must not follow it either: following it made
/// inotify watch whatever it pointed at — `/`, `$HOME` — and report changes
/// from outside the project as the project's.
///
/// The outside half can only fail on Linux: macOS's FSEvents never descends a
/// symlink, whatever the watcher is told, so there it passes by construction.
/// The setting itself is pinned on every platform by
/// `watch::tests::the_watcher_is_configured_not_to_follow_symlinks`; the inside
/// half (an edit in the project is still reported) holds everywhere.
#[cfg(unix)]
#[tokio::test]
async fn the_watcher_does_not_follow_a_symlink_out_of_the_project() {
    let (mut server, mut rx) = server();
    let project = temp_project("watch-symlink");
    let outside = TempDir::new("watch-outside");
    std::os::unix::fs::symlink(&*outside, project.join("ext")).unwrap();
    let root = std::fs::canonicalize(&*project).unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;
    settle_watcher(&mut rx, &root).await;

    std::fs::write(outside.join("elsewhere.rs"), "fn elsewhere() {}\n").unwrap();
    let seen = collect_for(&mut rx, Duration::from_secs(2)).await;
    for msg in &seen {
        if let ServerMessage::Notification {
            event: Event::FilesChanged { rels, .. },
            ..
        } = msg
        {
            assert!(
                rels.iter().all(|r| !r.starts_with("ext/")),
                "a change outside the project was reported: {rels:?}"
            );
        }
    }

    // A change inside is reported.
    std::fs::write(root.join("src/lib.rs"), "pub fn inside_marker() {}\n").unwrap();
    recv_note(&mut rx, "the in-project edit", |event| match event {
        Event::FilesChanged { rels, .. } if rels.iter().any(|r| r == "src/lib.rs") => Some(()),
        _ => None,
    })
    .await;
}

/// The watcher's noise filter rejects a path if ANY component is a build/VCS
/// name, so it has to run on the path RELATIVE to the root. Applied to the
/// absolute path, a project that merely LIVES under such a directory had every
/// one of its events classified as noise: an in-place edit published nothing at
/// all, and the symbols stayed at their open-time contents.
#[tokio::test]
async fn edits_publish_under_a_root_whose_ancestry_looks_like_noise() {
    let (mut server, mut rx) = server();
    // This bug needs the root itself to sit inside a directory the filter
    // rejects, so the project is built one level down under `node_modules`.
    let base = temp_project("watch-noisy-root");
    let root = base.join("node_modules/project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn before_marker() {}\n").unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;

    // An in-place rewrite of an existing file: no create, no rename, so the
    // changed rel is the ONLY thing that can carry this change to the client.
    std::fs::write(root.join("src/lib.rs"), "pub fn edited_marker() {}\n").unwrap();
    recv_note(&mut rx, "the edit's FilesChanged", |event| match event {
        Event::FilesChanged { rels, .. } if rels.iter().any(|r| r == "src/lib.rs") => Some(()),
        _ => None,
    })
    .await;
}

/// An in-place write to `.gitignore` changes which files belong to the project
/// without creating or removing anything. Deriving `structural` from the event
/// kind alone missed it, so the scanner kept applying the old rules and a
/// newly-ignored file stayed in the published set until the project was
/// reopened.
#[tokio::test]
async fn an_in_place_gitignore_edit_rescans_the_project() {
    let (mut server, mut rx) = server();
    let project = temp_project("watch-gitignore");
    let root = std::fs::canonicalize(&*project).unwrap();
    // The scanner's `ignore` walker honors `.gitignore` only inside a
    // repository, so the rules need a real one to have any effect.
    git(&root, &["init", "-q"]);
    std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(root.join("src/soon_ignored.rs"), "fn soon_ignored() {}\n").unwrap();

    let files = open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        files.iter().any(|f| f == "src/soon_ignored.rs"),
        "the file starts out part of the project: {files:?}"
    );

    // Appended in place — the case atomic-write editors and `git checkout` do
    // NOT produce, and the only one that reaches the watcher as `Modify(Data)`.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(".gitignore"))
            .unwrap();
        writeln!(f, "src/soon_ignored.rs").unwrap();
    }
    recv_note(
        &mut rx,
        "a Tree without the newly ignored file",
        |event| match event {
            Event::Tree { files, .. }
                if files.iter().any(|f| f == "src/lib.rs")
                    && !files.iter().any(|f| f == "src/soon_ignored.rs") =>
            {
                Some(())
            }
            _ => None,
        },
    )
    .await;
}

/// What a scan left out or added beyond the plain `.gitignore` walk reaches
/// the client as a `Status` notice, right behind the `Tree` it describes — at
/// open, and again after the watcher's rescan. It used to reach only the
/// server's log: a remote project listed a file its own `.gitignore` hides,
/// or silently lacked entries it could not read, and nothing said so.
#[tokio::test]
async fn the_scan_report_follows_the_tree_as_a_status_notice() {
    let (mut server, mut rx) = server();
    let project = temp_project("scan-report");
    let root = std::fs::canonicalize(&*project).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    // Committed, then hidden by the repository's own ignore rules: git still
    // tracks it, so the scan lists it anyway — and must say so.
    std::fs::write(root.join(".gitignore"), "src/util.py\n").unwrap();
    let expected = "1 tracked file shown despite .gitignore or a build-directory name";

    hello(&mut server).await;
    let open = Request::OpenProject {
        root: root.to_string_lossy().into_owned(),
    };
    assert!(server.handle(1, open).await.is_none());
    // Everything up to the notice, in arrival order: the Tree comes first.
    let mut seen = Vec::new();
    let notice = wait_for(&mut rx, "the open's scan notice", WAIT, |msg| {
        let found = match &msg {
            ServerMessage::Notification {
                event: Event::Status { message },
            } => Some(message.clone()),
            _ => None,
        };
        seen.push(msg);
        found
    })
    .await;
    assert_eq!(notice, expected);
    let tree_at = seen.iter().position(|m| {
        matches!(m, ServerMessage::Reply { id: 1, event: Event::Tree { files, .. } }
            if files.iter().any(|f| f == "src/util.py"))
    });
    assert!(
        tree_at.is_some_and(|at| at + 1 < seen.len()),
        "the notice follows the Tree that lists the file"
    );
    // And the Tree names the file, so the client can mark it: only the
    // count crossed before, in the notice.
    let marked = |tracked_ignored: &[String]| tracked_ignored == ["src/util.py"];
    assert!(
        tree_at.is_some_and(|at| matches!(
            &seen[at],
            ServerMessage::Reply { event: Event::Tree { tracked_ignored, .. }, .. }
                if marked(tracked_ignored)
        )),
        "the open's Tree does not name the tracked-but-ignored file"
    );

    // A structural change: the watcher rescans, and says it again after the
    // fresh tree — which names the file too.
    std::fs::write(root.join("src/fresh.rs"), "fn fresh() {}\n").unwrap();
    let rescanned = recv_note(&mut rx, "the rescan's tree", |event| match event {
        Event::Tree {
            files,
            tracked_ignored,
            ..
        } if files.iter().any(|f| f == "src/fresh.rs") => Some(tracked_ignored),
        _ => None,
    })
    .await;
    assert!(
        marked(&rescanned),
        "the rescan's Tree does not name the tracked-but-ignored file: {rescanned:?}"
    );
    let again = recv_note(&mut rx, "the rescan's scan notice", |event| match event {
        Event::Status { message } => Some(message),
        _ => None,
    })
    .await;
    assert_eq!(again, expected);
}

/// A plain walk has nothing to report, and sends no notice.
#[tokio::test]
async fn a_plain_scan_sends_no_status_notice() {
    let (mut server, mut rx) = server();
    let project = temp_project("scan-plain");
    open_project(&mut server, &mut rx, 1, &project).await;
    let after = collect_for(&mut rx, Duration::from_millis(500)).await;
    assert!(
        !after.iter().any(|m| matches!(
            m,
            ServerMessage::Notification {
                event: Event::Status { .. }
            }
        )),
        "{after:?}"
    );
}

/// A watcher event for a DIRECTORY names the directory, not the files under
/// it. Publishing that rel alone updated nothing: the old path's descendants
/// kept their stale symbols and the new path's were never read, so a renamed
/// folder left the index describing a tree that no longer existed.
#[tokio::test]
async fn renaming_a_directory_updates_its_descendants_symbols() {
    let (mut server, mut rx) = server();
    let project = temp_project("watch-dir-rename");
    let root = std::fs::canonicalize(&*project).unwrap();
    std::fs::create_dir_all(root.join("src/old")).unwrap();
    std::fs::write(root.join("src/old/moved.rs"), "fn moved_marker() {}\n").unwrap();
    open_project(&mut server, &mut rx, 1, &root).await;

    std::fs::rename(root.join("src/old"), root.join("src/new")).unwrap();

    // Both sides must be published: the vacated path cleared, the new one read.
    let (mut cleared, mut added) = (false, false);
    while !(cleared && added) {
        recv_note(
            &mut rx,
            "the rename's descendants republished",
            |event| match event {
                Event::ProjectSymbols { files, full, .. } => {
                    for f in &files {
                        if f.rel == "src/old/moved.rs" && f.symbols.is_empty() {
                            cleared = true;
                        }
                        if f.rel == "src/new/moved.rs"
                            && f.symbols.iter().any(|s| s.name == "moved_marker")
                        {
                            added = true;
                        }
                    }
                    // A full snapshot replaces everything, so the absence of the
                    // old path is what "cleared" means there.
                    if full && !files.iter().any(|f| f.rel == "src/old/moved.rs") {
                        cleared = true;
                    }
                    Some(())
                }
                _ => None,
            },
        )
        .await;
    }
}

// -- language servers ----------------------------------------------------------

/// A repo-specified LSP `command` runs only when the client pushed a matching
/// approval — the gate every spawn path shares. Without one, SpawnLsp refuses;
/// after `LspApprovals` with the fingerprint from `LspResolve`, it spawns.
#[tokio::test]
async fn repo_lsp_command_needs_a_pushed_approval() {
    let (mut server, mut rx) = server();
    let root = temp_project("lsp-approval");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    // A benign script standing in for the repo's command.
    script(&root.join("fake-lsp.sh"), "#!/bin/sh\nexit 0\n");
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
        matches!(refused, Event::Error { ref message, .. } if message.contains("not approved")),
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
    recv_note(&mut rx, "the approved spawn's exit", |event| match event {
        Event::ProcessExited { proc: 8, .. } => Some(()),
        _ => None,
    })
    .await;

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

/// `SpawnLsp` for a language with no server configured is answered like every
/// other spawn that will not run — an `Error` under its id, besides the
/// `ProcessExited` that ends the proxy. It used to end the proxy with no reply
/// at all, which "answered like `SpawnProcess`" promised it would not.
#[tokio::test]
async fn spawn_lsp_for_a_language_without_a_server_is_answered() {
    let (mut server, mut rx) = server();
    let root = temp_project("spawn-lsp-unconfigured");
    open_project(&mut server, &mut rx, 1, &root).await;
    let spawn = Request::SpawnLsp {
        proc: 11,
        language: "klingon".into(),
    };
    assert!(server.handle(2, spawn).await.is_none());
    let (mut exited, mut reply) = (false, None);
    wait_for(&mut rx, "the SpawnLsp reply and exit", WAIT, |msg| {
        match msg {
            ServerMessage::Notification {
                event: Event::ProcessExited { proc: 11, .. },
            } => exited = true,
            ServerMessage::Reply { id: 2, event } => reply = Some(event),
            _ => {}
        }
        (exited && reply.is_some()).then_some(())
    })
    .await;
    assert!(
        matches!(
            reply,
            Some(Event::Error { code: ErrorCode::Refused, ref message })
                if message.contains("no klingon language server")
        ),
        "{reply:?}"
    );
}

/// `SpawnLsp` for a store-managed server that is NOT installed must refuse —
/// never download. Installs happen only on `LspInstall`, the request that
/// carries the user's consent; `LspResolve` reports the install as pending so
/// the client can raise that consent prompt.
#[tokio::test]
async fn spawn_lsp_never_installs_without_consent() {
    let (mut server, mut rx) = server();
    let root = temp_project("no-autoinstall");
    open_project(&mut server, &mut rx, 1, &root).await;
    // Nothing in this binary installs a language server, so the shared data
    // dir holds no rust-analyzer: "rust" resolves to the store-managed
    // registry default, which is not installed.
    let installed = || data_dir().join("servers").join("rust-analyzer").exists();
    assert!(!installed(), "the fixture needs an empty store");

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
        matches!(refused, Event::Error { ref message, .. } if message.contains("not installed")),
        "an uninstalled server must refuse to spawn, got {refused:?}"
    );
    // …and nothing was downloaded behind the user's back.
    assert!(!installed(), "SpawnLsp must not install anything");

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
}

/// `LspInstall` is consent for the install the user SAW: it carries back the
/// `consent` digest of the offer. An install that is no longer that one — the
/// project pinned another version while the prompt was up — is refused: it
/// runs nothing, the user is told, and the reply is the current offer (with
/// its own digest) for a fresh prompt. (gopls installs through `go`, at the
/// version the project pins; resolving needs no toolchain, and nothing here
/// may reach the install that would.)
#[tokio::test]
async fn lsp_install_refuses_an_install_edited_after_the_prompt() {
    let (mut server, mut rx) = server();
    let root = temp_project("lsp-install-bound");
    open_project(&mut server, &mut rx, 1, &root).await;
    assert!(
        server
            .handle(
                2,
                Request::LspResolve {
                    language: "go".into(),
                },
            )
            .await
            .is_none()
    );
    let shown = match recv_reply(&mut rx, 2).await {
        Event::LspResolved {
            resolution: clew_protocol::LspResolution::NeedsInstall { consent, .. },
            ..
        } => consent,
        other => panic!("expected NeedsInstall, got {other:?}"),
    };

    // The repository pins another version while the prompt is up.
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[go]\nversion = \"v0.0.1-pinned-after-the-prompt\"\n",
    )
    .unwrap();
    assert!(
        server
            .handle(
                3,
                Request::LspInstall {
                    language: "go".into(),
                    consent: shown.clone(),
                },
            )
            .await
            .is_none()
    );
    let mut told = false;
    let reply = wait_for(&mut rx, "the install's reply", WAIT, |msg| match msg {
        ServerMessage::Reply { id: 3, event } => Some(event),
        ServerMessage::Notification {
            event: Event::Status { message },
        } => {
            told |= message.contains("nothing was installed");
            None
        }
        _ => None,
    })
    .await;
    match reply {
        Event::LspResolved {
            resolution:
                clew_protocol::LspResolution::NeedsInstall {
                    version, consent, ..
                },
            ..
        } => {
            assert_eq!(
                version, "v0.0.1-pinned-after-the-prompt",
                "the offer as it stands"
            );
            assert_ne!(consent, shown, "…with the digest of what would run now");
        }
        other => panic!("an install nobody saw must not run, got {other:?}"),
    }
    assert!(told, "the refusal must be reported, not silent");
    assert!(
        !data_dir().join("servers").join("gopls").exists(),
        "nothing may be installed for an install nobody approved"
    );
}

/// The Ask agent's semantic tools go through the same gate: an unapproved
/// repo command is refused, not executed.
#[tokio::test]
async fn agent_lsp_pool_honors_the_approval_gate() {
    let root = temp_project("agent-lsp-gate");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    script(&root.join("payload.sh"), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        root.join(".clew/lsp.toml"),
        "[rust]\ncommand = \"payload.sh\"\n",
    )
    .unwrap();

    let approvals = clew_server::SharedApprovals::default();
    let pool = clew_server::agent_lsp::LspPool::new(root.to_path_buf(), approvals.clone());
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

// -- proxied processes ---------------------------------------------------------

/// A spawned child that never reads its stdin must not wedge the request
/// loop: input is handed to a per-process writer task, so the pipe filling up
/// blocks nothing, and a following ProcessKill still gets through.
#[tokio::test]
async fn process_input_to_a_stalled_child_never_blocks_the_loop() {
    let (mut server, mut rx) = server();
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
    let pumped = tokio::time::timeout(Duration::from_secs(10), async {
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
    recv_note(&mut rx, "the killed child's exit", |event| match event {
        Event::ProcessExited { proc: 3, .. } => Some(()),
        _ => None,
    })
    .await;

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

/// `ReadFile` must bound what it reads BEFORE reading: an over-limit file is
/// refused by size, and a non-regular file (a FIFO would park the reader
/// forever) is refused by kind.
#[tokio::test]
#[cfg(unix)]
async fn read_file_refuses_oversized_and_non_regular_files() {
    let (mut server, mut rx) = server();
    let root = temp_project("read-bounds");
    // An over-limit "source file" (5 MB).
    std::fs::write(root.join("big.rs"), vec![b'a'; 5 * 1024 * 1024]).unwrap();
    // A FIFO: reading it would block forever.
    let fifo = root.join("pipe.rs");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo failed");
    open_project(&mut server, &mut rx, 1, &root).await;

    let read = |rel: &str| Request::ReadFile {
        rel: rel.into(),
        target: host_target(),
    };
    assert!(server.handle(2, read("big.rs")).await.is_none());
    match recv_reply(&mut rx, 2).await {
        Event::Error { message, .. } => assert!(message.contains("too large"), "{message}"),
        other => panic!("oversized read must error, got {other:?}"),
    }
    assert!(server.handle(3, read("pipe.rs")).await.is_none());
    match recv_reply(&mut rx, 3).await {
        // Opened without blocking, a FIFO is refused by the type check on
        // the handle, before any read: no regular file.
        Event::Error { message, .. } => assert!(message.contains("regular file"), "{message}"),
        other => panic!("FIFO read must error, got {other:?}"),
    }
}

/// Switching projects must not leave the previous project's language servers
/// or debug adapters running: OpenProject sweeps the whole process table (and
/// reports each death), in the same handler turn that switches the root.
#[tokio::test]
async fn open_project_kills_the_previous_projects_processes() {
    let (mut server, mut rx) = server();
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
    recv_note(
        &mut rx,
        "the old project's process exit",
        |event| match event {
            Event::ProcessExited { proc: 6, .. } => Some(()),
            _ => None,
        },
    )
    .await;
}

/// The client pipelines `ProcessInput` (an LSP `initialize`) right behind
/// `SpawnLsp`, whose resolve + spawn run on a detached task. Input sent in
/// that window must buffer and reach the child's stdin once it exists — the
/// stream starts at byte zero, never mid-way.
#[tokio::test]
async fn input_pipelined_behind_spawn_lsp_reaches_the_child() {
    let (mut server, mut rx) = server();
    let root = temp_project("spawn-race");
    std::fs::create_dir_all(root.join(".clew")).unwrap();
    // `cat` as a stand-in LSP: echoes stdin, so output proves delivery.
    script(&root.join("echo-lsp.sh"), "#!/bin/sh\nexec cat\n");
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
    let mut started = false;
    let mut output = Vec::new();
    while output.len() < 23 {
        recv_note(&mut rx, "the echoed input", |event| match event {
            Event::ProcessStarted { proc: 11 } => {
                started = true;
                Some(())
            }
            Event::ProcessOutput { proc: 11, data } => {
                assert!(started, "output before the ProcessStarted ack");
                output.extend_from_slice(&data);
                Some(())
            }
            Event::ProcessExited { proc: 11, .. } => panic!("child died before echoing"),
            _ => None,
        })
        .await;
    }
    assert_eq!(output, b"Content-Length: 2\r\n\r\n{}");
    server.handle(6, Request::ProcessKill { proc: 11 }).await;
}

/// A spawn that fails at the OS level must end the stream like any other
/// death: `ProcessExited` (so the client's proxy sees EOF and unmaps the
/// handle) plus the error itself — never the error alone.
#[tokio::test]
async fn failed_spawn_reports_process_exited() {
    let (mut server, mut rx) = server();
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
    recv_note(&mut rx, "the failed spawn's exit", |event| match event {
        Event::ProcessExited { proc: 4, .. } => Some(()),
        _ => None,
    })
    .await;
}

/// When a child stops reading and its stdin queue fills, the server must not
/// silently drop frames (one lost chunk desyncs Content-Length framing
/// forever): it kills the process and says so.
#[tokio::test]
async fn stdin_overflow_kills_the_process_loudly() {
    let (mut server, mut rx) = server();
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
        Some(Event::Error { message, .. }) => {
            assert!(message.contains("overflow"), "unexpected error: {message}")
        }
        other => panic!("overflow must produce an error, got {other:?}"),
    }
    // …and the child is gone, without any ProcessKill from the client.
    recv_note(
        &mut rx,
        "the overflowed process's exit",
        |event| match event {
            Event::ProcessExited { proc: 5, .. } => Some(()),
            _ => None,
        },
    )
    .await;
}

/// Proxied stdin crosses in chunks of at most the protocol's chunk size — the
/// client cuts its stream that way — so a bigger one is refused (with a code)
/// before it reaches the child, and ends the child: its stream would be
/// missing those bytes for good, so one left running would answer garbage (an
/// over-full queue is treated the same way). A full-size chunk goes through.
#[tokio::test]
async fn process_input_is_capped_at_the_protocol_chunk() {
    let (mut server, mut rx) = server();
    let root = temp_project("stdin-chunk-cap");
    open_project(&mut server, &mut rx, 1, &root).await;
    let spawn_cat = |proc| Request::SpawnProcess {
        proc,
        cmd: "cat".into(),
        args: vec![],
        cwd: None,
    };
    assert!(server.handle(2, spawn_cat(6)).await.is_none());
    let over = server
        .handle(
            3,
            Request::ProcessInput {
                proc: 6,
                data: vec![b'x'; clew_protocol::MAX_PROCESS_CHUNK + 1],
            },
        )
        .await;
    assert!(
        matches!(
            over,
            Some(Event::Error {
                code: ErrorCode::Refused,
                ..
            })
        ),
        "an over-cap chunk is refused, got {over:?}"
    );
    recv_note(
        &mut rx,
        "the desynced process's exit",
        |event| match event {
            Event::ProcessExited { proc: 6, .. } => Some(()),
            Event::ProcessOutput { proc: 6, .. } => panic!("the refused bytes reached the child"),
            _ => None,
        },
    )
    .await;

    assert!(server.handle(4, spawn_cat(7)).await.is_none());
    let full = vec![b'y'; clew_protocol::MAX_PROCESS_CHUNK];
    assert!(
        server
            .handle(
                5,
                Request::ProcessInput {
                    proc: 7,
                    data: full.clone(),
                },
            )
            .await
            .is_none(),
        "a full-size chunk is accepted"
    );
    // It reaches the child whole.
    let mut echoed = 0;
    while echoed < full.len() {
        echoed += recv_note(&mut rx, "the echoed chunk", |event| match event {
            Event::ProcessOutput { proc: 7, data } => {
                assert!(
                    data.iter().all(|&b| b == b'y'),
                    "foreign bytes in the stream"
                );
                Some(data.len())
            }
            _ => None,
        })
        .await;
    }
    assert_eq!(echoed, full.len());
    server.handle(6, Request::ProcessKill { proc: 7 }).await;
}

/// Closing stdout is not exiting. A child that closes it and keeps working
/// used to be reported dead — and, because the table entry owns the `Child`
/// with `kill_on_drop`, actually killed. The exit must be reported only when
/// the process really exits, with the status it really finished with.
#[tokio::test]
async fn closing_stdout_does_not_kill_a_running_child() {
    let (mut server, mut rx) = server();
    let root = temp_project("stdout-eof");
    open_project(&mut server, &mut rx, 1, &root).await;

    // Closes stdout immediately, keeps running, then exits with code 7.
    assert!(
        server
            .handle(
                2,
                Request::SpawnProcess {
                    proc: 12,
                    cmd: "sh".into(),
                    args: vec!["-c".into(), "exec 1>&-; sleep 1; exit 7".into()],
                    cwd: None,
                },
            )
            .await
            .is_none(),
        "spawn should succeed"
    );

    // Nothing may be reported while it is still running, even though stdout
    // hit EOF at once.
    let early = collect_for(&mut rx, Duration::from_millis(400)).await;
    assert!(
        !early.iter().any(|m| matches!(
            m,
            ServerMessage::Notification {
                event: Event::ProcessExited { proc: 12, .. },
                ..
            }
        )),
        "stdout EOF was reported as the process exiting"
    );

    // And the real exit arrives, carrying the real status.
    let code = recv_note(&mut rx, "the child's real exit", |event| match event {
        Event::ProcessExited { proc: 12, code } => Some(code),
        _ => None,
    })
    .await;
    assert_eq!(code, Some(7), "the exit status the child finished with");
}

/// The mirror image of the test above: the child exits promptly but a
/// descendant it spawned inherited stdout and keeps the pipe open. Waiting for
/// EOF before waiting for the child meant the exit was reported only when the
/// DESCENDANT finished — for a daemon, never.
///
/// The descendant waits on the process's stdin rather than sleeping, so it
/// ends as soon as the server retires the process and closes that pipe: no
/// orphan outlives the test.
#[tokio::test]
async fn a_descendant_holding_stdout_does_not_delay_the_exit() {
    let (mut server, mut rx) = server();
    let root = temp_project("stdout-inherited");
    open_project(&mut server, &mut rx, 1, &root).await;

    // The child exits with 3 at once; the background reader inherits stdout
    // and holds it until its stdin closes. (An asynchronous list's stdin is
    // /dev/null unless redirected explicitly — hence fd 3.)
    assert!(
        server
            .handle(
                2,
                Request::SpawnProcess {
                    proc: 21,
                    cmd: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "exec 3<&0; (read line <&3) & echo started; exit 3".into()
                    ],
                    cwd: None,
                },
            )
            .await
            .is_none(),
        "spawn should succeed"
    );
    let code = recv_note(&mut rx, "the child's exit", |event| match event {
        Event::ProcessExited { proc: 21, code } => Some(code),
        _ => None,
    })
    .await;
    assert_eq!(code, Some(3), "the exit status the child finished with");
}

/// A process nobody stopped that exits uncleanly is reported, with the end of
/// what it wrote to stderr — how a language server that cannot start looks.
/// Its stderr used to be discarded, so it died in silence. A clean exit and a
/// killed process say nothing.
#[tokio::test]
async fn a_process_that_dies_says_why() {
    let (mut server, mut rx) = server();
    let root = temp_project("stderr-report");
    open_project(&mut server, &mut rx, 1, &root).await;
    let spawn = |proc: u64, script: &str| Request::SpawnProcess {
        proc,
        cmd: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
    };

    assert!(
        server
            .handle(
                2,
                spawn(
                    4,
                    "echo loading >&2; echo 'fatal: no toolchain' >&2; exit 4"
                )
            )
            .await
            .is_none()
    );
    let mut report = None;
    wait_for(&mut rx, "the crash's exit", WAIT, |msg| match msg {
        // A notice, not an error reply: no request is waiting on it.
        ServerMessage::Notification {
            event: Event::Status { message },
        } => {
            report = Some(message);
            None
        }
        ServerMessage::Notification {
            event: Event::ProcessExited { proc: 4, code },
            ..
        } => Some(code),
        _ => None,
    })
    .await;
    let report = report.expect("an unclean exit is reported before the exit event");
    assert!(
        report.contains("exited with code 4") && report.contains("fatal: no toolchain"),
        "{report}"
    );

    // A killed process and a clean exit: no report. (Killed first, so no exit
    // can be skipped while waiting for the start.)
    assert!(server.handle(3, spawn(6, "exec sleep 30")).await.is_none());
    recv_note(&mut rx, "the running child", |event| match event {
        Event::ProcessStarted { proc: 6 } => Some(()),
        _ => None,
    })
    .await;
    server.handle(4, Request::ProcessKill { proc: 6 }).await;
    assert!(
        server
            .handle(5, spawn(5, "echo fine >&2; exit 0"))
            .await
            .is_none()
    );
    let (mut clean, mut killed) = (false, false);
    while !(clean && killed) {
        wait_for(&mut rx, "both exits", WAIT, |msg| match msg {
            ServerMessage::Notification {
                event: Event::Status { message },
            } => panic!("nothing to report here, got {message}"),
            ServerMessage::Notification {
                event: Event::ProcessExited { proc, .. },
                ..
            } => {
                clean |= proc == 5;
                killed |= proc == 6;
                Some(())
            }
            _ => None,
        })
        .await;
    }
}

/// Over SSH `SpawnProcess` — an arbitrary command line — is refused: a remote
/// client never needs it (remote language servers and debug adapters are
/// resolved and gated on the server), so keeping it would only keep a way to
/// run anything. The refusal ends the proxied stream like any failed spawn.
#[tokio::test]
async fn a_remote_server_refuses_spawn_process() {
    data_dir();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::with_policy(tx, SpawnPolicy::Remote);
    let root = temp_project("remote-spawn");
    open_project(&mut server, &mut rx, 1, &root).await;
    let refused = server
        .handle(
            2,
            Request::SpawnProcess {
                proc: 3,
                cmd: "sh".into(),
                args: vec!["-c".into(), "echo ran".into()],
                cwd: None,
            },
        )
        .await;
    assert!(
        matches!(refused, Some(Event::Error { ref message, .. }) if message.contains("refused")),
        "{refused:?}"
    );
    recv_note(&mut rx, "the refused spawn's exit", |event| match event {
        Event::ProcessExited { proc: 3, .. } => Some(()),
        Event::ProcessOutput { proc: 3, .. } => panic!("the command ran"),
        _ => None,
    })
    .await;
}

// -- model calls: cancellation -------------------------------------------------

/// Point the server at `provider`.
async fn use_provider(server: &mut Server, provider: &FakeProvider) {
    assert!(
        server
            .handle(
                90,
                Request::SetAiConfig {
                    chat: Some(provider.chat_config()),
                    embed: None,
                },
            )
            .await
            .is_none()
    );
}

/// A `Chat` whose model call failed says what failed, typed: the status the
/// provider answered with, its kind of error and its words — what a client
/// acts on, as it does on a call it made itself. It crossed as llm's words
/// for the failure, which the client parsed back.
#[tokio::test]
async fn a_chat_the_provider_refused_says_what_failed_typed() {
    let (mut server, mut rx) = server();
    hello(&mut server).await;
    let provider = FakeProvider::new(Pace::Refuse(
        "401 Unauthorized",
        r#"{"error":{"message":"Incorrect API key provided","code":"invalid_api_key"}}"#,
    ));
    use_provider(&mut server, &provider).await;
    let chat = Request::Chat {
        system: "s".into(),
        messages: vec![AiChatMsg {
            role: "user".into(),
            content: "hi".into(),
        }],
        max_tokens: 64,
    };
    assert!(server.handle(71, chat).await.is_none());
    let reply = wait_for(&mut rx, "the chat's reply", WAIT, |msg| match msg {
        ServerMessage::Reply { id: 71, event, .. } => Some(event),
        _ => None,
    })
    .await;
    let Event::Error {
        code: ErrorCode::Provider(failure),
        message,
    } = reply
    else {
        panic!("not a typed provider failure: {reply:?}");
    };
    assert_eq!(
        failure,
        ProviderFailure::Status {
            code: 401,
            kind: Some("invalid_api_key".into()),
            message: "Incorrect API key provided".into(),
        }
    );
    assert!(message.contains("401"), "{message}");
}

/// An abandoned streamed answer must stop where it runs. The client dropping
/// its pump reaches nothing on the server, so without a Cancel the provider
/// call ran to completion on the meter with nobody listening. Here a real
/// stream is running: Cancel ends it at once, and the provider sees the
/// connection close long before its answer was done.
#[tokio::test]
async fn cancel_stops_a_streamed_chat() {
    let (mut server, mut rx) = server();
    hello(&mut server).await;
    let provider = FakeProvider::new(Pace::Stream {
        every: Duration::from_millis(100),
        max: 100,
    });
    use_provider(&mut server, &provider).await;

    // Cancel for an id nobody registered is a no-op, not an error: the work
    // may simply have finished first.
    assert!(
        server
            .handle(2, Request::Cancel { id: 4242 })
            .await
            .is_none()
    );

    assert!(
        server
            .handle(
                3,
                Request::ChatStream {
                    stream: 41,
                    system: "s".into(),
                    messages: vec![AiChatMsg {
                        role: "user".into(),
                        content: "hi".into(),
                    }],
                    max_tokens: 64,
                },
            )
            .await
            .is_none()
    );
    recv_note(&mut rx, "the first streamed token", |event| match event {
        Event::ChatDelta { stream: 41, .. } => Some(()),
        _ => None,
    })
    .await;
    assert!(server.handle(4, Request::Cancel { id: 41 }).await.is_none());
    let outcome = wait_for(
        &mut rx,
        "the cancelled stream's end",
        Duration::from_secs(3),
        |msg| match msg {
            ServerMessage::Notification {
                event:
                    Event::ChatStreamDone {
                        stream: 41,
                        outcome,
                    },
                ..
            } => Some(outcome),
            _ => None,
        },
    )
    .await;
    // A Stop is its own ending, not a failure the client has to recognize
    // by its wording ("cancelled" once read as "Ask failed: cancelled").
    assert_eq!(outcome, StreamOutcome::Stopped);
    let sent = provider
        .hung_up
        .recv_timeout(Duration::from_secs(5))
        .expect("the provider must see the connection close");
    assert!(sent < 100, "the generation was cut short ({sent} tokens)");
}

/// A non-streamed `Chat` is stoppable too, by its request id: the caller is
/// answered at once instead of holding a thread (and the provider meter)
/// until the whole answer arrived.
#[tokio::test]
async fn cancel_answers_a_blocking_chat_at_once() {
    let (mut server, mut rx) = server();
    hello(&mut server).await;
    let provider = FakeProvider::new(Pace::Silent(Duration::from_secs(10)));
    use_provider(&mut server, &provider).await;
    assert!(
        server
            .handle(
                61,
                Request::Chat {
                    system: "s".into(),
                    messages: vec![AiChatMsg {
                        role: "user".into(),
                        content: "hi".into(),
                    }],
                    max_tokens: 64,
                },
            )
            .await
            .is_none()
    );
    let wait = tokio::task::spawn_blocking({
        let requests = provider.requests.clone();
        move || {
            let began = Instant::now();
            while requests.load(Ordering::SeqCst) == 0 {
                assert!(began.elapsed() < WAIT, "the provider was never called");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    wait.await.unwrap();
    assert!(
        server
            .handle(62, Request::Cancel { id: 61 })
            .await
            .is_none()
    );
    let reply = wait_for(
        &mut rx,
        "the cancelled chat's reply",
        Duration::from_secs(3),
        |msg| match msg {
            ServerMessage::Reply { id: 61, event, .. } => Some(event),
            _ => None,
        },
    )
    .await;
    assert!(
        matches!(
            reply,
            Event::Error {
                code: ErrorCode::Cancelled,
                ..
            }
        ),
        "a cancelled Chat is answered as cancelled, not as a failure: {reply:?}"
    );
}

/// An agent turn whose model request is on the wire stops at `Cancel` (by its
/// stream id): the turn closes as stopped within moments, and the provider
/// sees its connection dropped.
#[tokio::test(flavor = "multi_thread")]
async fn agent_stop_ends_a_turn_mid_request() {
    let (mut server, mut rx) = server();
    let root = temp_project("agent-stop");
    open_project(&mut server, &mut rx, 1, &root).await;
    let provider = FakeProvider::new(Pace::Stream {
        every: Duration::from_millis(100),
        max: 100,
    });
    use_provider(&mut server, &provider).await;
    assert!(
        server
            .handle(
                2,
                Request::AgentAsk {
                    stream: 51,
                    question: "what does add do?".into(),
                    history: Vec::new(),
                    context: String::new(),
                },
            )
            .await
            .is_none()
    );
    let started = tokio::task::spawn_blocking({
        let requests = provider.requests.clone();
        move || {
            let began = Instant::now();
            while requests.load(Ordering::SeqCst) == 0 {
                assert!(began.elapsed() < WAIT, "the model was never called");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    started.await.unwrap();
    assert!(server.handle(3, Request::Cancel { id: 51 }).await.is_none());
    let outcome = wait_for(
        &mut rx,
        "the stopped turn's AgentDone",
        Duration::from_secs(3),
        |msg| match msg {
            ServerMessage::Notification {
                event:
                    Event::AgentDone {
                        stream: 51,
                        outcome,
                    },
                ..
            } => Some(outcome),
            _ => None,
        },
    )
    .await;
    assert_eq!(outcome, StreamOutcome::Stopped);
    let sent = provider
        .hung_up
        .recv_timeout(Duration::from_secs(5))
        .expect("the provider must see the connection close");
    assert!(sent < 100, "the step was cut short ({sent} tokens)");
}

// -- the whole process ---------------------------------------------------------

/// Talks to a real `clew-server` process over its stdio.
struct Process {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<ServerMessage>,
}

impl Process {
    fn spawn() -> Process {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_clew-server"))
            // Launched as the app launches its local server: told it is
            // local, so it is one whatever shell runs the tests (see
            // SpawnPolicy).
            .arg("--local")
            .env("CLEW_DATA_DIR", data_dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the clew-server binary runs");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Ok(msg) = serde_json::from_str::<ServerMessage>(&line)
                    && tx.send(msg).is_err()
                {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Process {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, id: u64, request: Request) {
        let mut line = serde_json::to_string(&ClientMessage { id, request }).unwrap();
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn wait_for<T>(&self, what: &str, mut pick: impl FnMut(ServerMessage) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(msg) => {
                    if let Some(found) = pick(msg) {
                        return found;
                    }
                }
                Err(_) => panic!("no {what} from the server process"),
            }
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// When the client goes away, the server stops everything it started and the
/// PROCESS exits — promptly, even with an agent turn blocked inside a model
/// request that will never answer. It used to stop nothing: the turn kept
/// running for a client that was gone, and the runtime's shutdown then waited
/// on that blocked thread with no limit, keeping the process (and its
/// children) alive.
#[cfg(unix)]
#[test]
fn a_disconnect_stops_the_work_and_ends_the_process() {
    let root = temp_project("disconnect");
    let provider = FakeProvider::new(Pace::Silent(Duration::from_secs(60)));
    let mut server = Process::spawn();

    server.send(
        1,
        Request::Hello {
            protocol: PROTOCOL_VERSION,
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        },
    );
    server.wait_for("Ready", |msg| match msg {
        ServerMessage::Reply {
            id: 1,
            event: Event::Ready { .. },
            ..
        } => Some(()),
        ServerMessage::Reply { id: 1, event, .. } => panic!("handshake refused: {event:?}"),
        _ => None,
    });
    server.send(
        2,
        Request::OpenProject {
            root: root.to_string_lossy().into_owned(),
        },
    );
    server.wait_for("the Tree", |msg| match msg {
        ServerMessage::Reply {
            id: 2,
            event: Event::Tree { .. },
            ..
        } => Some(()),
        _ => None,
    });
    // A child that reports its pid, then lives for 30s unless killed.
    server.send(
        3,
        Request::SpawnProcess {
            proc: 9,
            cmd: "sh".into(),
            args: vec!["-c".into(), "echo $$; exec sleep 30".into()],
            cwd: None,
        },
    );
    let pid: i32 = server.wait_for("the child's pid", |msg| match msg {
        ServerMessage::Notification {
            event: Event::ProcessOutput { proc: 9, data },
            ..
        } => String::from_utf8_lossy(&data).trim().parse().ok(),
        _ => None,
    });
    server.send(
        4,
        Request::SetAiConfig {
            chat: Some(provider.chat_config()),
            embed: None,
        },
    );
    server.send(
        5,
        Request::AgentAsk {
            stream: 7,
            question: "what does add do?".into(),
            history: Vec::new(),
            context: String::new(),
        },
    );
    provider.await_request();

    // The client goes away.
    drop(server.stdin.take());
    let began = Instant::now();
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            began.elapsed() < Duration::from_secs(15),
            "the server process is still running after its client left"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "a clean exit: {status:?}");

    // The model request's connection went with it...
    assert!(
        provider
            .hung_up
            .recv_timeout(Duration::from_secs(5))
            .is_ok(),
        "the model request must not outlive the server"
    );
    // ...and so did the child it had started.
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let began = Instant::now();
    while alive() {
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "the child process {pid} outlived the disconnect"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
