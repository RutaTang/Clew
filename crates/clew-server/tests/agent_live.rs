//! Live smoke test of the whole Ask agent loop against the machine's
//! configured LLM provider (global clew config). Exercises the real tool
//! loop end to end: exploration steps, the `answer` tool, and the streamed
//! final answer. Costs a few API tokens; ignored by default.
//!
//! Run with:
//! `cargo test -p clew-server --test agent_live -- --ignored --nocapture`

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clew_core::fs_scan::FileEntry;
use clew_protocol::{Event, ServerMessage};
use clew_server::{agent, agent_lsp};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "makes a real LLM API call using the configured provider"]
async fn ask_agent_streams_a_grounded_answer() {
    let Some(chat) = clew_core::llm::Config::load() else {
        eprintln!("no LLM provider configured; nothing to smoke-test");
        return;
    };

    let dir = std::env::temp_dir().join("clew-agent-live-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// Adds two numbers, saturating at the top.\n\
         pub fn add(a: u32, b: u32) -> u32 {\n    a.saturating_add(b)\n}\n",
    )
    .unwrap();
    let files = Arc::new(vec![FileEntry {
        abs: dir.join("src/lib.rs"),
        rel: "src/lib.rs".into(),
    }]);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let pool = Arc::new(agent_lsp::LspPool::new(dir.clone()));
    let rt = tokio::runtime::Handle::current();
    let stop = Arc::new(AtomicBool::new(false));
    let root = dir.clone();
    let stop2 = stop.clone();
    tokio::task::spawn_blocking(move || {
        agent::run(
            root,
            files,
            chat,
            None,
            pool,
            rt,
            1,
            "What does the function in src/lib.rs do on overflow? Answer in one sentence.".into(),
            Vec::new(),
            String::new(),
            &tx,
            &stop2,
        );
    })
    .await
    .unwrap();

    let mut deltas = 0;
    let mut steps = 0;
    let mut done_err = None;
    let mut answer = String::new();
    while let Ok(msg) = rx.try_recv() {
        if let ServerMessage::Notification { event, .. } = msg {
            match event {
                Event::AgentStep { title, .. } => {
                    steps += 1;
                    eprintln!("step: {title}");
                }
                Event::AgentDelta { text, .. } => {
                    deltas += 1;
                    answer.push_str(&text);
                }
                Event::AgentDone { error, .. } => done_err = error,
                _ => {}
            }
        }
    }
    eprintln!("steps={steps} deltas={deltas}");
    eprintln!("answer: {answer}");
    assert_eq!(done_err, None, "turn closed cleanly");
    assert!(steps > 0, "the model explored before answering");
    assert!(!answer.trim().is_empty(), "an answer was produced");
}
