//! End-to-end test of the agent's semantic tools against a real language
//! server: builds a tiny fixture crate, lets `LspPool` spawn the managed
//! rust-analyzer, and checks definition / references / hover land correctly.
//!
//! Ignored by default: it needs the managed rust-analyzer installed (the
//! store under the clew data dir) and takes a few seconds to index.
//! Run with: `cargo test -p clew-server --test agent_lsp -- --ignored`

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use clew_core::testutil::TempDir;
use clew_server::agent_lsp::{LspPool, Semantic};

/// Write the fixture crate into `dir` (created), returning it.
fn fixture_at(dir: &std::path::Path) -> PathBuf {
    let dir = dir.to_path_buf();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "mod util;\n\npub fn caller() -> u32 {\n    util::helper()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/util.rs"),
        "/// Returns the answer.\npub fn helper() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    dir
}

#[tokio::test]
#[ignore = "needs the managed rust-analyzer installed; spawns a real server"]
async fn definition_references_and_hover_resolve() {
    let scratch = TempDir::new("agent-lsp-e2e");
    let root = fixture_at(&scratch.join("fixture"));
    let pool = LspPool::new(root.clone(), Default::default());
    let stop = AtomicBool::new(false);

    // Definition of `helper` at its call site resolves into util.rs.
    let def = pool
        .query(
            Semantic::Definition,
            "src/lib.rs",
            &root.join("src/lib.rs"),
            4,
            "helper",
            &stop,
        )
        .await
        .expect("definition query");
    assert!(
        def.content.contains("src/util.rs:2"),
        "definition lands on the fn: {}",
        def.content
    );

    // References of `helper` from its definition include the call site.
    let refs = pool
        .query(
            Semantic::References,
            "src/util.rs",
            &root.join("src/util.rs"),
            2,
            "helper",
            &stop,
        )
        .await
        .expect("references query");
    assert!(
        refs.content.contains("src/lib.rs:4"),
        "references include the caller: {}",
        refs.content
    );

    // Hover shows the signature.
    let hover = pool
        .query(
            Semantic::Hover,
            "src/lib.rs",
            &root.join("src/lib.rs"),
            4,
            "helper",
            &stop,
        )
        .await
        .expect("hover query");
    assert!(
        hover.content.contains("fn helper"),
        "hover shows the signature: {}",
        hover.content
    );
}

/// Regression: a symlinked project root (like `/tmp` → `/private/tmp` on
/// macOS) must still yield project-relative targets, and an on-disk edit
/// after the first query must be re-synced to the server (`didChange`), not
/// answered against a stale overlay.
#[cfg(unix)]
#[tokio::test]
#[ignore = "needs the managed rust-analyzer installed; spawns a real server"]
async fn symlinked_root_and_on_disk_edits_resolve() {
    let scratch = TempDir::new("agent-lsp-symlink");
    let real = fixture_at(&scratch.join("real"));
    let link = scratch.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let pool = LspPool::new(link.clone(), Default::default());
    let stop = AtomicBool::new(false);

    // Through the symlink, targets still come back project-relative (both in
    // the text the model reads and in the client's chips).
    let def = pool
        .query(
            Semantic::Definition,
            "src/lib.rs",
            &link.join("src/lib.rs"),
            4,
            "helper",
            &stop,
        )
        .await
        .expect("definition query");
    assert!(
        def.content.contains("src/util.rs:2"),
        "relative path through a symlinked root: {}",
        def.content
    );
    assert!(
        def.targets.contains(&("src/util.rs".to_string(), 2)),
        "chip is project-relative: {:?}",
        def.targets
    );

    // Edit util.rs on disk: a new doc line shifts `helper` to line 3. The
    // follow-up query anchors on the NEW line numbers — it only resolves if
    // the pool re-synced the changed text.
    std::fs::write(
        real.join("src/util.rs"),
        "//! Utility functions.\n/// Returns the answer.\npub fn helper() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    let refs = pool
        .query(
            Semantic::References,
            "src/util.rs",
            &link.join("src/util.rs"),
            3,
            "helper",
            &stop,
        )
        .await
        .expect("references query after on-disk edit");
    assert!(
        refs.content.contains("src/lib.rs:4"),
        "references resolve against the fresh text: {}",
        refs.content
    );
}

/// Regression: a doc left open by an earlier query must not go stale. The
/// pool used to `didChange` only the file the current query named, so once
/// util.rs had been queried, editing it on disk and then asking about
/// *lib.rs* was answered against util.rs's pre-edit overlay — and only
/// self-healed the next time util.rs itself was the queried file.
#[tokio::test]
#[ignore = "needs the managed rust-analyzer installed; spawns a real server"]
async fn edits_to_other_open_docs_resync_before_the_next_query() {
    let scratch = TempDir::new("agent-lsp-crossfile");
    let root = fixture_at(&scratch.join("fixture"));
    let pool = LspPool::new(root.clone(), Default::default());
    let stop = AtomicBool::new(false);

    // First query is about util.rs, which leaves it open on the server with
    // `helper` on line 2.
    let refs = pool
        .query(
            Semantic::References,
            "src/util.rs",
            &root.join("src/util.rs"),
            2,
            "helper",
            &stop,
        )
        .await
        .expect("references query");
    assert!(
        refs.content.contains("src/lib.rs:4"),
        "references include the caller: {}",
        refs.content
    );

    // Edit util.rs on disk, pushing `helper` down to line 3, then query a
    // DIFFERENT file. Nothing about this query mentions util.rs, so the
    // definition only lands on the new line if the pool re-synced every open
    // doc rather than just the one being asked about.
    std::fs::write(
        root.join("src/util.rs"),
        "//! Utility functions.\n/// Returns the answer.\npub fn helper() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    let def = pool
        .query(
            Semantic::Definition,
            "src/lib.rs",
            &root.join("src/lib.rs"),
            4,
            "helper",
            &stop,
        )
        .await
        .expect("definition query after editing another open file");
    assert!(
        def.content.contains("src/util.rs:3"),
        "definition reflects the edited util.rs: {}",
        def.content
    );
    assert!(
        !def.content.contains("src/util.rs:2"),
        "no answer from the pre-edit overlay: {}",
        def.content
    );
    assert!(
        def.targets.contains(&("src/util.rs".to_string(), 3)),
        "chip points at the new line: {:?}",
        def.targets
    );
}
