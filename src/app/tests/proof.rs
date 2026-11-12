//! Rust's and Lean's own reading aids: a type's memory layout and an
//! item's tests from rust-analyzer (`app::insight`), and a Lean proof
//! stepped through one tactic at a time (`app::proof`), each asked of a
//! scripted language server.

use super::*;
use crate::ProofMsg;
use clew_core::lsp;

/// A language server stub: answers `initialize`, then each request by
/// `answer(method, params)`, recording every frame the client sends.
async fn scripted(
    root: &std::path::Path,
    answer: fn(&str, &serde_json::Value) -> serde_json::Value,
) -> (
    lsp::client::LspClient,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    let (client_stdin, mut peer_rx) = tokio::io::duplex(64 * 1024);
    let (mut peer_tx, client_stdout) = tokio::io::duplex(64 * 1024);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
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
                let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else {
                    continue;
                };
                recorder.lock().unwrap().push(msg.clone());
                let Some(id) = msg.get("id").cloned() else {
                    continue;
                };
                let method = msg["method"].as_str().unwrap_or("");
                let result = match method {
                    "initialize" => serde_json::json!({"capabilities": {}}),
                    _ => answer(method, &msg["params"]),
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
    (client, seen)
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

/// rust-analyzer's answers as 1.95 gives them for `Packet` and for
/// `double`'s test.
fn rust_analyzer(method: &str, _: &serde_json::Value) -> serde_json::Value {
    match method {
        "rust-analyzer/viewRecursiveMemoryLayout" => serde_json::json!({"nodes": [
            {"itemName": "[ROOT]", "typename": "Packet", "size": 16, "offset": 0, "alignment": 8, "parentIdx": -1, "childrenStart": 1, "childrenLen": 3},
            {"itemName": "id", "typename": "u64", "size": 8, "offset": 0, "alignment": 8, "parentIdx": 0, "childrenStart": -1, "childrenLen": 0},
            {"itemName": "tag", "typename": "u16", "size": 2, "offset": 8, "alignment": 2, "parentIdx": 0, "childrenStart": -1, "childrenLen": 0},
            {"itemName": "flag", "typename": "u8", "size": 1, "offset": 10, "alignment": 1, "parentIdx": 0, "childrenStart": -1, "childrenLen": 0}]}),
        "rust-analyzer/relatedTests" => serde_json::json!([{"runnable": {
            "label": "test tests::doubles",
            "location": {"targetUri": "file:///nonexistent/elsewhere.rs",
                "targetSelectionRange": {"start": {"line": 3, "character": 7}, "end": {"line": 3, "character": 14}}},
            "kind": "cargo", "args": {}}}]),
        _ => serde_json::Value::Null,
    }
}

/// A Rust file's context menu offers its memory layout and its tests;
/// each is asked of rust-analyzer and shown as the popup says it: the
/// layout as rustc placed the fields, padding counted, and each test a
/// link into the project.
#[tokio::test(flavor = "multi_thread")]
async fn a_rust_type_shows_its_layout_and_an_item_its_tests() {
    let mut app = scanned_app("rust-layout-tests");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub struct Packet { pub flag: u8, pub id: u64, pub tag: u16 }\n\npub fn double(x: u32) -> u32 { x * 2 }\n\n#[test]\nfn doubles() { assert_eq!(double(2), 4); }\n",
    )
    .unwrap();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "src/lib.rs", None);
    assert_eq!(
        crate::InsightKind::more_for_lang("rust"),
        [
            crate::InsightKind::MemoryLayout,
            crate::InsightKind::RelatedTests
        ]
    );
    assert!(crate::InsightKind::more_for_lang("python").is_empty());
    let (client, seen) = scripted(&root, rust_analyzer).await;
    app.proj
        .link
        .lsp
        .insert("rust".into(), LspSlot::Ready(client));

    let ask = |app: &mut App, kind, line, col| {
        open_menu(app, line, col);
        app.update(Message::Hover(HoverMsg::InsightOf(kind)))
    };
    let answer = |messages: Vec<Message>| {
        messages.into_iter().find_map(|m| match m {
            Message::Hover(HoverMsg::InsightAnswered { result, .. }) => Some(result),
            _ => None,
        })
    };
    let task = ask(&mut app, crate::InsightKind::MemoryLayout, 0, 12);
    assert_eq!(
        app.proj.insight.as_ref().map(|i| i.title.as_str()),
        Some("Memory layout of Packet")
    );
    let layout = answer(run_task_async(task).await)
        .expect("answered")
        .expect("asked")
        .expect("a layout");
    let md = layout.markdown;
    assert!(md.contains("16 bytes, aligned to 8"), "{md}");
    assert!(md.contains("5 bytes of padding"), "{md}");
    // As rustc placed them: id first, flag last, then the padding.
    let order: Vec<usize> = ["  id ", "  tag ", "  flag ", "(padding)"]
        .iter()
        .map(|w| md.find(w).unwrap_or_else(|| panic!("{w} in {md}")))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{md}");
    let asked = seen.lock().unwrap().clone();
    assert!(
        asked
            .iter()
            .any(|m| m["method"] == "rust-analyzer/viewRecursiveMemoryLayout"
                && m["params"]["position"] == serde_json::json!({"line": 0, "character": 12}))
    );

    let task = ask(&mut app, crate::InsightKind::RelatedTests, 2, 8);
    let tests = answer(run_task_async(task).await)
        .expect("answered")
        .expect("asked")
        .expect("tests");
    // Out of the project: named, not linked.
    assert!(
        tests.markdown.contains("- `tests::doubles`"),
        "{}",
        tests.markdown
    );
    let linked = crate::app::insight::tests_answer(
        &[lsp::client::RelatedTest {
            name: "tests::doubles".into(),
            path: root.join("src/lib.rs"),
            line: 5,
        }],
        Some(&root),
    )
    .unwrap();
    assert!(
        linked
            .markdown
            .contains("[`tests::doubles`](clew:src/lib.rs:6)"),
        "{}",
        linked.markdown
    );
    assert!(crate::app::insight::tests_answer(&[], Some(&root)).is_none());
}

/// A Lean proof of four tactics and its start.
const PROOF: &str = "theorem and_swap (p q : Prop) (hp : p) (hq : q) : q ∧ p := by\n  constructor\n  · exact hq\n  · exact hp\n\ndef answer : Nat := 42\n";

/// The Lean server's goals after each line of [`PROOF`], by the line asked
/// at.
fn lean_server(method: &str, params: &serde_json::Value) -> serde_json::Value {
    if method != "$/lean/plainGoal" {
        return serde_json::Value::Null;
    }
    let goals: Vec<&str> = match params["position"]["line"].as_u64() {
        Some(0) => vec!["p q : Prop\nhp : p\nhq : q\n⊢ q ∧ p"],
        Some(1) => vec![
            "case left\np q : Prop\nhp : p\nhq : q\n⊢ q",
            "case right\np q : Prop\nhp : p\nhq : q\n⊢ p",
        ],
        Some(2) => vec!["case right\np q : Prop\nhp : p\nhq : q\n⊢ p"],
        Some(3) => vec![],
        _ => return serde_json::Value::Null,
    };
    serde_json::json!({ "goals": goals })
}

/// Step Through Proof asks the Lean server for the goals after each
/// tactic — at its end, in the server's units — and walks them: the
/// start, then each step, its line lit, back and on, to the proof's end.
#[tokio::test(flavor = "multi_thread")]
async fn a_lean_proof_steps_through_its_tactics() {
    let mut app = scanned_app("lean-proof-steps");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(root.join("Swap.lean"), PROOF).unwrap();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "Swap.lean", None);
    let abs = root.join("Swap.lean");

    // No server yet: said, and nothing opened.
    open_menu(&mut app, 2, 4);
    let _ = app.update(Message::Proof(ProofMsg::FromMenu));
    assert!(app.proj.proof.is_none());
    assert!(app.status.contains("Lean server"), "{}", app.status);

    let (client, seen) = scripted(&root, lean_server).await;
    app.proj
        .link
        .lsp
        .insert("lean".into(), LspSlot::Ready(client));
    // Not in a tactic proof: said.
    open_menu(&mut app, 5, 4);
    let _ = app.update(Message::Proof(ProofMsg::FromMenu));
    assert!(app.proj.proof.is_none());
    assert!(
        app.status.contains("No tactic proof here"),
        "{}",
        app.status
    );

    open_menu(&mut app, 2, 4);
    let task = app.update(Message::Proof(ProofMsg::FromMenu));
    let walk = app.proj.proof.as_ref().expect("a walk");
    assert_eq!(walk.name, "and_swap");
    assert_eq!(walk.steps.len(), 4, "{:?}", walk.steps);
    assert!(walk.goals.is_none(), "asked, not answered");
    assert!(app.show_bottom);
    assert_eq!(app.proof_line(&abs), None, "nothing lit before the answer");
    for m in run_task_async(task).await {
        let _ = app.update(m);
    }
    let walk = app.proj.proof.as_ref().unwrap();
    assert_eq!(walk.at, 0);
    assert_eq!(walk.after(0).map(<[String]>::len), Some(1));
    assert_eq!(walk.after(1).map(<[String]>::len), Some(2));
    assert_eq!(walk.after(3), Some(&[][..]), "proved at the end");
    assert_eq!(app.proof_line(&abs), Some(1));

    // Asked at each step's end, in UTF-16 units: the first line's `∧` is
    // three bytes and one unit.
    let first_line = PROOF.lines().next().unwrap();
    let asked: Vec<serde_json::Value> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "$/lean/plainGoal")
        .map(|m| m["params"]["position"].clone())
        .collect();
    assert_eq!(
        asked,
        [
            serde_json::json!({"line": 0, "character": first_line.encode_utf16().count()}),
            serde_json::json!({"line": 1, "character": 13}),
            serde_json::json!({"line": 2, "character": 12}),
            serde_json::json!({"line": 3, "character": 12}),
        ]
    );

    let step = |app: &mut App, msg| {
        let _ = app.update(Message::Proof(msg));
        app.proj.proof.as_ref().unwrap().at
    };
    assert_eq!(
        step(&mut app, ProofMsg::Step(false)),
        0,
        "nothing before the start"
    );
    assert_eq!(step(&mut app, ProofMsg::Step(true)), 1);
    assert_eq!(app.proof_line(&abs), Some(2));
    assert_eq!(step(&mut app, ProofMsg::Goto(3)), 3);
    assert_eq!(
        step(&mut app, ProofMsg::Step(true)),
        3,
        "nothing past the end"
    );
    assert_eq!(step(&mut app, ProofMsg::Goto(9)), 3, "no such step");
    // A debug session's line is the one lit while it runs.
    let _ = app.update(Message::Proof(ProofMsg::Close));
    assert!(app.proj.proof.is_none());
    assert_eq!(app.proof_line(&abs), None);
}

/// An answer to an earlier asking lands nowhere: Step Through Proof asked
/// again since.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_answer_does_not_replace_the_newer_proof() {
    let mut app = scanned_app("lean-proof-late");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(root.join("Swap.lean"), PROOF).unwrap();
    scan_synchronously(&mut app, root.clone());
    open_synchronously(&mut app, "Swap.lean", None);
    let (client, _seen) = scripted(&root, lean_server).await;
    app.proj
        .link
        .lsp
        .insert("lean".into(), LspSlot::Ready(client));
    open_menu(&mut app, 1, 2);
    let first = app.update(Message::Proof(ProofMsg::FromMenu));
    open_menu(&mut app, 1, 2);
    let _second = app.update(Message::Proof(ProofMsg::FromMenu));
    for m in run_task_async(first).await {
        let _ = app.update(m);
    }
    assert!(
        app.proj.proof.as_ref().unwrap().goals.is_none(),
        "the first asking's answer was taken for the second's"
    );
}

/// The palette offers Rust's and Lean's own actions for a file of theirs,
/// and not for another's.
#[test]
fn the_palette_offers_each_languages_own_actions() {
    let mut app = scanned_app("lang-actions-palette");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(root.join("Swap.lean"), PROOF).unwrap();
    scan_synchronously(&mut app, root);
    let titles = |app: &App| -> Vec<String> {
        app.palette_commands()
            .into_iter()
            .map(|c| c.title.to_string())
            .collect()
    };
    open_synchronously(&mut app, "src/lib.rs", None);
    app.active_viewer_mut().unwrap().caret = Some((0, 0));
    let rust = titles(&app);
    assert!(rust.iter().any(|t| t == "Memory Layout"), "{rust:?}");
    assert!(rust.iter().any(|t| t == "Related Tests"));
    assert!(!rust.iter().any(|t| t == "Step Through Proof"));
    open_synchronously(&mut app, "Swap.lean", None);
    app.active_viewer_mut().unwrap().caret = Some((1, 2));
    let lean = titles(&app);
    assert!(lean.iter().any(|t| t == "Step Through Proof"), "{lean:?}");
    assert!(!lean.iter().any(|t| t == "Memory Layout"));
}

/// A gap between two fields is shown where it is, between them, and a
/// type that ends on its last field has no padding after it.
#[test]
fn padding_between_fields_is_shown_between_them() {
    use clew_core::lsp::client::{LayoutNode, MemoryLayout};
    let node = |name: &str, typename: &str, size, offset, parent| LayoutNode {
        name: name.into(),
        typename: typename.into(),
        size,
        offset,
        alignment: size.max(1),
        parent,
    };
    let layout = MemoryLayout {
        nodes: vec![
            node("[ROOT]", "Pair", 8, 0, None),
            node("a", "u8", 1, 0, Some(0)),
            node("b", "u32", 4, 4, Some(0)),
        ],
    };
    let md = crate::app::insight::layout_answer(&layout).markdown;
    let rows: Vec<&str> = md
        .lines()
        .skip_while(|l| !l.starts_with("```"))
        .skip(2)
        .take_while(|l| !l.starts_with("```"))
        .collect();
    assert_eq!(rows.len(), 3, "{md}");
    assert!(rows[0].contains(" a ") && rows[0].contains("u8"), "{md}");
    assert!(
        rows[1].trim_start().starts_with("1") && rows[1].contains("(padding)"),
        "{md}"
    );
    assert!(rows[2].contains(" b ") && rows[2].contains("u32"), "{md}");
    assert!(md.contains("3 bytes of padding"), "{md}");
}
