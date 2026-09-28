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
    // Opt-in (`--ignored`), so an unconfigured machine is a failure to report,
    // not a silent pass that looks like a working agent.
    let Some(chat) = clew_core::llm::Config::load() else {
        panic!("no LLM provider configured — set one in clew's settings to run this smoke test");
    };

    let scratch = clew_core::testutil::TempDir::new("agent-live");
    let dir = scratch.to_path_buf();
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
    let pool = Arc::new(agent_lsp::LspPool::new(dir.clone(), Default::default()));
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
            &clew_server::OutputBudget::new(),
            &stop2,
        );
    })
    .await
    .unwrap();

    let mut deltas = 0;
    let mut steps = 0;
    let mut done = None;
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
                Event::AgentDone { outcome, .. } => done = Some(outcome),
                _ => {}
            }
        }
    }
    eprintln!("steps={steps} deltas={deltas}");
    eprintln!("answer: {answer}");
    assert_eq!(
        done,
        Some(clew_protocol::StreamOutcome::Done),
        "turn closed cleanly"
    );
    assert!(steps > 0, "the model explored before answering");
    assert!(!answer.trim().is_empty(), "an answer was produced");
}
