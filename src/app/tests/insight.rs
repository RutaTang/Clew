//! The context menu's language actions (`app::insight`): a macro's
//! expansion, a Lean goal, a Haskell type — asked of Clew's own C reader,
//! locally and on a remote host, and of each language's server.

use super::*;
use crate::app::insight::{c_answer, haskell_answer, lean_answer, rust_answer};
use clew_core::lsp::client::{ExpandedMacro, LeanGoal};

/// A project with a C file whose macro comes from a header, scanned and
/// the file open.
fn c_project(tag: &str) -> (App, clew_core::testutil::TempDir, PathBuf) {
    let dir = clew_core::testutil::TempDir::new(tag);
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("include")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("include/util.h"),
        "#define MAX(a, b) ((a) > (b) ? (a) : (b))\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.c"),
        "#include \"util.h\"\nint m = MAX(1, 2);\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "plain\n").unwrap();
    let mut app = blank_app();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/main.c", None);
    (app, dir, root)
}

fn open_menu(app: &mut App, line: usize, col: usize) {
    let pane = app.proj.active;
    let _ = app.update(Message::Hover(HoverMsg::ContextMenuOpened {
        pane,
        line,
        col,
        x: 10.0,
        y: 10.0,
    }));
}

fn answered(m: &Message) -> bool {
    matches!(m, Message::Hover(HoverMsg::InsightAnswered { .. }))
}

#[test]
fn each_language_has_its_own_action() {
    for (lang, kind) in [
        ("rust", Some(InsightKind::ExpandMacro)),
        ("c", Some(InsightKind::ExpandMacro)),
        ("cpp", Some(InsightKind::ExpandMacro)),
        ("lean", Some(InsightKind::GoalState)),
        ("haskell", Some(InsightKind::TypeAtPoint)),
        ("python", None),
        ("go", None),
    ] {
        assert_eq!(InsightKind::for_lang(lang), kind, "{lang}");
    }
    assert_eq!(InsightKind::ExpandMacro.label(), "Expand Macro");
    assert_eq!(InsightKind::GoalState.label(), "Show Goal");
    assert_eq!(InsightKind::TypeAtPoint.label(), "Show Type");
    // The menu offers it for the file's language, and not for a file with
    // none.
    let (mut app, _dir, _root) = c_project("insight-menu");
    open_menu(&mut app, 1, 8);
    assert_eq!(
        app.proj.context_menu.as_ref().and_then(|m| m.insight),
        Some(InsightKind::ExpandMacro)
    );
    open_synchronously(&mut app, "notes.txt", None);
    open_menu(&mut app, 0, 0);
    assert_eq!(app.proj.context_menu.as_ref().and_then(|m| m.insight), None);
}

/// A C macro expands here, from the header the file includes; the popup
/// opens the definition, and a late answer to an earlier ask lands nowhere.
#[test]
fn a_c_macro_expands_locally_and_opens_its_definition() {
    let (mut app, _dir, root) = c_project("insight-local-c");
    open_menu(&mut app, 1, 8);
    let task = app.update(Message::Hover(HoverMsg::Insight));
    assert!(app.proj.context_menu.is_none(), "the menu closes");
    let open = app.proj.insight.as_ref().expect("the popup opens");
    assert!(open.loading);
    assert_eq!(open.title, "Expansion of MAX");
    let messages = run_task(task);
    let answer = messages.iter().find_map(|m| match m {
        Message::Hover(HoverMsg::InsightAnswered { result, .. }) => Some(result.clone()),
        _ => None,
    });
    let expected = c_answer(clew_protocol::MacroExpansion {
        name: "MAX".into(),
        invocation: "MAX(1, 2)".into(),
        expanded: "((1) > (2) ? (1) : (2))".into(),
        file: "include/util.h".into(),
        line: 1,
        definition: "#define MAX(a, b) ((a) > (b) ? (a) : (b))".into(),
        truncated: false,
    });
    assert_eq!(answer, Some(Ok(Some(expected))));
    for m in messages {
        if answered(&m) {
            let _ = app.update(m);
        }
    }
    let open = app.proj.insight.as_ref().unwrap();
    assert!(!open.loading);
    assert!(!open.prepared.is_empty());
    assert_eq!(open.defined, Some(("include/util.h".to_string(), 1)));
    // Open Definition: the header, at the #define; the popup closes.
    let _ = app.update(Message::Hover(HoverMsg::InsightOpenDefinition));
    assert!(app.proj.insight.is_none());
    land_active_load(&mut app, root.join("include/util.h"), Some(1));
    assert_eq!(
        app.active_viewer().map(|v| v.rel.as_str()),
        Some("include/util.h")
    );

    // Asked twice: only the last one's answer is shown.
    open_synchronously(&mut app, "src/main.c", None);
    open_menu(&mut app, 1, 8);
    let first = app.update(Message::Hover(HoverMsg::Insight));
    let first_token = app.proj.insight.as_ref().unwrap().token;
    open_menu(&mut app, 1, 0);
    let _second = app.update(Message::Hover(HoverMsg::Insight));
    assert_eq!(app.proj.insight.as_ref().unwrap().title, "Expansion of int");
    for m in run_task(first) {
        if let Message::Hover(HoverMsg::InsightAnswered { token, .. }) = &m {
            assert_eq!(*token, first_token);
            let _ = app.update(m);
        }
    }
    assert!(
        app.proj.insight.as_ref().unwrap().loading,
        "the earlier answer does not land in the later popup"
    );
    let _ = app.update(Message::Hover(HoverMsg::InsightClose));
    assert!(app.proj.insight.is_none());

    // `ge` asks at the caret.
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.caret = Some((1, 9));
    }
    let task = app.update(Message::Hover(HoverMsg::InsightAtCursor));
    assert_eq!(app.proj.insight.as_ref().unwrap().title, "Expansion of MAX");
    drive(&mut app, task, answered);
    assert_eq!(
        app.proj.insight.as_ref().unwrap().defined,
        Some(("include/util.h".to_string(), 1))
    );
}

/// A remote project's C macro is expanded on its host: the request names
/// the file, the line and the byte the cursor is at, and the reading target.
#[test]
fn a_remote_c_macro_is_expanded_on_its_host() {
    let (mut app, root, mut rx) = remote_app("insight-remote-c");
    std::fs::write(root.join("main.c"), "int m = MAX(1, 2);\n").unwrap();
    open_synchronously(&mut app, "main.c", None);
    open_menu(&mut app, 0, 8);
    let pending = app.server.ai_pending.clone();
    let task = app.update(Message::Hover(HoverMsg::Insight));
    let running = std::thread::spawn(move || run_task(task));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut asked = None;
    while !running.is_finished() {
        assert!(std::time::Instant::now() < deadline, "never answered");
        while let Ok(msg) = rx.try_recv() {
            if let clew_protocol::Request::ExpandMacro {
                rel,
                line,
                column,
                target,
            } = msg.request
            {
                asked = Some((rel.clone(), line, column, target.os.clone()));
                let reply = clew_protocol::Event::MacroExpansion {
                    rel,
                    line,
                    column,
                    expansion: Some(clew_protocol::MacroExpansion {
                        name: "MAX".into(),
                        invocation: "MAX(1, 2)".into(),
                        expanded: "((1) > (2) ? (1) : (2))".into(),
                        file: "util.h".into(),
                        line: 4,
                        definition: "#define MAX(a, b) ((a) > (b) ? (a) : (b))".into(),
                        truncated: false,
                    }),
                };
                if let Some(tx) = crate::app::rpc::lock_replies(&pending).remove(&msg.id) {
                    let _ = tx.send(reply);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(asked, Some(("main.c".into(), 0, 8, app.target_spec().os)));
    for m in running.join().unwrap() {
        if answered(&m) {
            let _ = app.update(m);
        }
    }
    let open = app.proj.insight.as_ref().unwrap();
    assert!(!open.loading);
    assert_eq!(open.defined, Some(("util.h".to_string(), 4)));
}

/// What the language server answers is what the popup shows, for Rust,
/// Lean and Haskell alike; with no server running it says so at once.
#[tokio::test(flavor = "multi_thread")]
async fn rust_lean_and_haskell_ask_their_language_server() {
    let mut app = scanned_app("insight-lsp");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(root.join("src/lib.rs"), "fn f() { let v = vec![1, 2]; }\n").unwrap();
    std::fs::write(root.join("Proof.lean"), "theorem t : 1 = 1 := by\n  rfl\n").unwrap();
    std::fs::write(
        root.join("Main.hs"),
        "main :: IO ()\nmain = print (foo 1)\n",
    )
    .unwrap();
    scan_synchronously(&mut app, root.clone());

    // No server: the popup says what is missing, not "Asking…".
    open_synchronously(&mut app, "src/lib.rs", None);
    app.proj
        .link
        .lsp
        .insert("rust".into(), LspSlot::Failed("exited".into()));
    open_menu(&mut app, 0, 17);
    let _ = app.update(Message::Hover(HoverMsg::Insight));
    let open = app.proj.insight.as_ref().unwrap();
    assert!(!open.loading, "no server to wait for");
    assert!(!open.prepared.is_empty());
    let not_running = format!("{:?}", open.prepared);
    assert!(not_running.contains("is not running"), "{not_running}");
    // Still starting: says so, and asks nothing.
    app.proj.link.lsp.insert("rust".into(), LspSlot::Starting);
    open_menu(&mut app, 0, 17);
    let _ = app.update(Message::Hover(HoverMsg::Insight));
    let starting = format!("{:?}", app.proj.insight.as_ref().unwrap().prepared);
    assert!(starting.contains("is still starting"), "{starting}");

    let (client, wire) = scripted_lsp_client(&root).await;
    for lang in ["rust", "lean", "haskell"] {
        app.proj
            .link
            .lsp
            .insert(lang.into(), LspSlot::Ready(client.clone()));
    }
    let cases: [(&str, usize, usize, &str, InsightAnswer); 3] = [
        (
            "src/lib.rs",
            0,
            17,
            "rust-analyzer/expandMacro",
            rust_answer(ExpandedMacro {
                name: "vec!".into(),
                expansion: "<[_]>::into_vec(Box::new([1, 2]))".into(),
            }),
        ),
        (
            "Proof.lean",
            1,
            2,
            "$/lean/plainGoal",
            lean_answer(LeanGoal::Tactic(vec!["⊢ 1 = 1".into()])),
        ),
        (
            "Main.hs",
            1,
            15,
            "textDocument/hover",
            haskell_answer("```haskell\nfoo :: Int -> Int\n```\n\n*Defined at Lib.hs:3:1*")
                .unwrap(),
        ),
    ];
    for (rel, line, col, method, want) in cases {
        if app.active_viewer().is_none_or(|v| v.rel != rel) {
            open_synchronously(&mut app, rel, None);
        }
        open_menu(&mut app, line, col);
        let task = app.update(Message::Hover(HoverMsg::Insight));
        let messages = run_task_async(task).await;
        let got = messages.iter().find_map(|m| match m {
            Message::Hover(HoverMsg::InsightAnswered { result, .. }) => Some(result.clone()),
            _ => None,
        });
        assert_eq!(got, Some(Ok(Some(want))), "{rel}");
        for m in messages {
            if answered(&m) {
                let _ = app.update(m);
            }
        }
        assert!(!app.proj.insight.as_ref().unwrap().loading, "{rel}");
        let seen = wire.lock().unwrap().clone();
        let asked = format!("\"method\":\"{method}\"");
        assert!(seen.contains(&asked), "{rel}: {method} in {seen}");
        assert!(
            seen.contains(&format!(
                "\"position\":{{\"character\":{col},\"line\":{line}}}"
            )) || seen.contains(&format!(
                "\"position\":{{\"line\":{line},\"character\":{col}}}"
            )),
            "{rel}: the position asked about"
        );
    }
}

/// A language server stub: answers `initialize`, then each request by its
/// method — rust-analyzer's expansion, the Lean goal, a Haskell hover — and
/// records every frame the client sends.
async fn scripted_lsp_client(
    root: &std::path::Path,
) -> (lsp::client::LspClient, Arc<std::sync::Mutex<String>>) {
    let (client_stdin, mut peer_rx) = tokio::io::duplex(64 * 1024);
    let (mut peer_tx, client_stdout) = tokio::io::duplex(64 * 1024);
    let wire = Arc::new(std::sync::Mutex::new(String::new()));
    let recorder = wire.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut pending: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match peer_rx.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            pending.extend_from_slice(&buf[..n]);
            // Each whole frame received so far.
            while let Some(head_end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&pending[..head_end]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if pending.len() < head_end + 4 + len {
                    break;
                }
                let body = pending[head_end + 4..head_end + 4 + len].to_vec();
                pending.drain(..head_end + 4 + len);
                let text = String::from_utf8_lossy(&body).to_string();
                recorder.lock().unwrap().push_str(&text);
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(id) = msg.get("id").cloned() else {
                    continue;
                };
                let result = match msg["method"].as_str().unwrap_or("") {
                    "initialize" => serde_json::json!({"capabilities": {}}),
                    "rust-analyzer/expandMacro" => serde_json::json!({
                        "name": "vec!",
                        "expansion": "<[_]>::into_vec(Box::new([1, 2]))"
                    }),
                    "$/lean/plainGoal" => serde_json::json!({
                        "rendered": "```lean\n⊢ 1 = 1\n```",
                        "goals": ["⊢ 1 = 1"]
                    }),
                    "textDocument/hover" => serde_json::json!({
                        "contents": {
                            "kind": "markdown",
                            "value": "```haskell\nfoo :: Int -> Int\n```\n\n*Defined at Lib.hs:3:1*"
                        }
                    }),
                    _ => serde_json::Value::Null,
                };
                let reply =
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
                let framed = format!("Content-Length: {}\r\n\r\n{reply}", reply.len());
                if peer_tx.write_all(framed.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
    });
    let client = lsp::client::LspClient::connect(client_stdin, client_stdout, root, None)
        .await
        .expect("stub handshake");
    (client, wire)
}

#[test]
fn answers_read_as_the_popup_shows_them() {
    let rust = rust_answer(ExpandedMacro {
        name: "vec!".into(),
        expansion: "<[_]>::into_vec(Box::new([1]))\n".into(),
    });
    assert_eq!(
        rust.markdown,
        "`vec!` expands to\n\n```rust\n<[_]>::into_vec(Box::new([1]))\n```"
    );
    assert_eq!(rust.defined, None);

    let full = c_answer(clew_protocol::MacroExpansion {
        name: "N".into(),
        invocation: "N".into(),
        expanded: "10".into(),
        file: "a.h".into(),
        line: 2,
        definition: "#define N 10".into(),
        truncated: true,
    });
    assert_eq!(
        full.markdown,
        "```c\nN\n```\n\nexpands to\n\n```c\n10\n```\n\nfrom this definition\n\n```c\n#define N 10\n```\n\nDefined at `a.h:2`.\n\n*Cut short: the expansion grew past clew's limit.*"
    );
    assert_eq!(full.defined, Some(("a.h".into(), 2)));
    let flag = c_answer(clew_protocol::MacroExpansion {
        name: "LEVEL".into(),
        invocation: "LEVEL".into(),
        expanded: "3".into(),
        definition: "#define LEVEL 3".into(),
        ..Default::default()
    });
    assert!(flag.markdown.ends_with("Defined by the compile command."));
    assert_eq!(flag.defined, None);
    let line = c_answer(clew_protocol::MacroExpansion {
        invocation: "int x = N;".into(),
        expanded: "int x = 10;".into(),
        ..Default::default()
    });
    assert_eq!(
        line.markdown,
        "```c\nint x = N;\n```\n\nexpands to\n\n```c\nint x = 10;\n```"
    );

    assert_eq!(
        lean_answer(LeanGoal::Tactic(Vec::new())).markdown,
        "*No goals: the proof is complete here.*"
    );
    assert_eq!(
        lean_answer(LeanGoal::Tactic(vec!["⊢ p".into()])).markdown,
        "```lean\n⊢ p\n```"
    );
    assert_eq!(
        lean_answer(LeanGoal::Tactic(vec!["⊢ p".into(), "⊢ q".into()])).markdown,
        "**Goal 1 of 2**\n\n```lean\n⊢ p\n```\n\n**Goal 2 of 2**\n\n```lean\n⊢ q\n```"
    );
    assert_eq!(
        lean_answer(LeanGoal::Term("⊢ Nat".into())).markdown,
        "Expected type\n\n```lean\n⊢ Nat\n```"
    );

    let hs = haskell_answer("```haskell\nfoo :: Int\n```\n\n*Defined at A.hs:1*").unwrap();
    assert_eq!(
        hs.markdown,
        "```haskell\nfoo :: Int\n```\n\n*Defined at A.hs:1*"
    );
    assert_eq!(
        haskell_answer("x :: Bool").unwrap().markdown,
        "```haskell\nx :: Bool\n```"
    );
    assert_eq!(haskell_answer("   "), None);
    // As haskell-language-server answers for `print` given an Int: the type
    // at this use under the declared one; the separators and the `_ ::`
    // blocks are not shown.
    let print = haskell_answer("```haskell\nprint :: forall a. Show a => a -> IO ()\n```\n\n*Defined in System.IO*\n\n* * *\n\n```haskell\n_ :: Int -> IO ()\n```\n* * *\n\n```haskell\n_ :: forall a. Show a => a -> IO ()\n```").unwrap();
    assert_eq!(
        print.markdown,
        "```haskell\nprint :: forall a. Show a => a -> IO ()\n```\n\nHere: `Int -> IO ()`\n\n*Defined in System.IO*"
    );
}
