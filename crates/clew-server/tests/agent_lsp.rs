//! End-to-end test of the agent's semantic tools against a real language
//! server: builds a tiny fixture crate, lets `LspPool` spawn the managed
//! rust-analyzer, and checks definition / references / hover land correctly.
//!
//! Ignored by default: it needs the managed rust-analyzer installed (the
//! store under the clew data dir) and takes a few seconds to index.
//! Run with: `cargo test -p clew-server --test agent_lsp -- --ignored`

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use clew_server::agent_lsp::{LspPool, Semantic};

fn fixture_at(dir: &std::path::Path) -> PathBuf {
    let dir = dir.to_path_buf();
    let _ = std::fs::remove_dir_all(&dir);
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

fn fixture() -> PathBuf {
    fixture_at(&std::env::temp_dir().join("clew-agent-lsp-e2e"))
}

#[tokio::test]
#[ignore = "needs the managed rust-analyzer installed; spawns a real server"]
async fn definition_references_and_hover_resolve() {
    let root = fixture();
    let pool = LspPool::new(root.clone());
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
    let real = fixture_at(&std::env::temp_dir().join("clew-agent-lsp-symlink-real"));
    let link = std::env::temp_dir().join("clew-agent-lsp-symlink-link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let pool = LspPool::new(link.clone());
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
