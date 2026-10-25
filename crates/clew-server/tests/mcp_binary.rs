//! `clew-server mcp` end to end: the real binary, reached by clew's own MCP
//! client the way any MCP client reaches it — a process, spoken to over its
//! stdio.
//!
//! Hermetic: the project and the data directory are throwaway directories of
//! this run (`CLEW_DATA_DIR` is set for the child only), and the server's
//! language servers are never needed by what is asked.

use std::time::Duration;

use clew_core::linerpc::Launch;
use clew_core::mcp::{Approve, Client, Era, MODERN_VERSION, ServerConfig, Transport};
use clew_core::testutil::TempDir;

fn config(project: &std::path::Path, data: &std::path::Path) -> ServerConfig {
    ServerConfig {
        name: "clew".into(),
        transport: Transport::Stdio(Launch {
            program: env!("CARGO_BIN_EXE_clew-server").into(),
            args: vec![
                "mcp".into(),
                "--root".into(),
                project.to_string_lossy().into_owned(),
            ],
            env: vec![("CLEW_DATA_DIR".into(), data.to_string_lossy().into_owned())],
            cwd: None,
        }),
        enabled: true,
        approve: Approve::Ask,
        allow: Vec::new(),
        timeout: Duration::from_secs(30),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_serves_its_code_tools_over_mcp() {
    let project = TempDir::new("mcp-bin-project");
    let data = TempDir::new("mcp-bin-data");
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(
        project.path().join("src/lib.rs"),
        "/// Adds one.\npub fn add_one(a: u32) -> u32 {\n    a + 1\n}\n",
    )
    .unwrap();
    let client = Client::connect(&config(project.path(), data.path()))
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    assert_eq!(client.info().era, Era::Modern(MODERN_VERSION.into()));
    assert_eq!(client.info().name, "clew");
    assert!(
        client
            .info()
            .instructions
            .as_deref()
            .is_some_and(|i| i.contains("project-relative")),
        "{:?}",
        client.info()
    );
    let tools = client.tools().await.unwrap();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no {name} tool"))
    };
    assert!(tools.iter().all(|t| t.read_only));
    let found = client
        .call_tool(
            tool("search"),
            serde_json::json!({"query": "a + 1"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(found.text.contains("src/lib.rs:3"), "{}", found.text);
    let read = client
        .call_tool(
            tool("read"),
            serde_json::json!({"file": "src/lib.rs"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(read.text.contains("pub fn add_one"), "{}", read.text);
    let outline = client
        .call_tool(
            tool("outline"),
            serde_json::json!({"file": "src/lib.rs"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(outline.text.contains("add_one"), "{}", outline.text);
    client.close().await;
    assert!(!client.alive());
}

#[test]
fn a_bad_root_is_refused_with_usage() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_clew-server"))
        .args(["mcp", "--root", "/definitely/not/a/dir"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout carries MCP messages only");
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: clew-server mcp"));
}
