//! `clew-server mcp`: clew's code tools as an MCP server on stdio, for any
//! agent that speaks MCP — Claude Code, an editor's assistant, or an ACP agent
//! clew itself started for the Ask panel.
//!
//! The tools are the Ask agent's own ([`crate::agent::code_tools`]): exact and
//! semantic search, reading and outlining files, language-server navigation
//! (definition, references, hover), git history and the current changes, and
//! the project's cached explanations. All of them only read, and every path
//! they take is confined to the project, as in a turn.
//!
//! Run from the project (or with `--root <dir>`), it serves that project until
//! the client closes its input. The file list is rescanned when it is a few
//! seconds old, so files created meanwhile are found. Language servers the
//! project's own `lsp.toml` names are not started — that needs the user's
//! approval, which only the app can ask for — but the built-in ones are.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use clew_core::fs_scan::FileEntry;
use clew_core::mcp::ToolOutput;
use clew_core::mcp::serve::{Host, Tool};
use clew_core::{embed, fs_scan};

use crate::agent;
use crate::agent_lsp::LspPool;

/// How old the file list may be before a call rescans it.
const FILES_FRESH_FOR: Duration = Duration::from_secs(5);

/// The project the server serves, and what its tools share.
struct CodeTools {
    root: PathBuf,
    files: Mutex<(Instant, Arc<Vec<FileEntry>>)>,
    lsp: Arc<LspPool>,
    rt: tokio::runtime::Handle,
}

impl CodeTools {
    /// The project's files, rescanned when the last scan is stale.
    fn files(&self) -> Arc<Vec<FileEntry>> {
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        if files.0.elapsed() > FILES_FRESH_FOR {
            *files = (
                Instant::now(),
                Arc::new(fs_scan::scan(self.root.clone()).files),
            );
        }
        files.1.clone()
    }
}

impl Host for CodeTools {
    fn identity(&self) -> (String, String, Option<String>) {
        let name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (
            "clew".into(),
            env!("CARGO_PKG_VERSION").into(),
            Some(format!(
                "Code-intelligence tools for the project \"{name}\" ({}). Paths are \
                 project-relative. Prefer `definition`/`references`/`hover` (language-server \
                 precise) over `search` when tracing symbols; `outline` maps a file before \
                 you `read` it; `changes` and `history` explain what changed and why.",
                self.root.display()
            )),
        )
    }

    fn tools(&self) -> Vec<Tool> {
        agent::code_tools()
            .into_iter()
            .map(|t| Tool {
                name: t.name,
                title: None,
                description: t.description,
                input_schema: t.parameters,
            })
            .collect()
    }

    fn call(
        &self,
        name: &str,
        args: &serde_json::Value,
        stop: &AtomicBool,
    ) -> Result<ToolOutput, String> {
        let text = agent::run_code_tool(
            self.root.clone(),
            self.files(),
            embed::Config::load(),
            self.lsp.clone(),
            self.rt.clone(),
            name,
            args,
            stop,
        )
        .ok_or_else(|| format!("unknown tool: {name}"))?;
        Ok(ToolOutput {
            text,
            is_error: false,
        })
    }
}

/// The project root `args` name (`--root <dir>`), else the working directory.
pub fn root_from_args(args: &[String]) -> Result<PathBuf, String> {
    let named = args
        .iter()
        .position(|a| a == "--root")
        .map(|i| args.get(i + 1).cloned().ok_or("--root needs a directory"))
        .transpose()?;
    let root = match named {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?,
    };
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    Ok(root)
}

/// Serve `root`'s tools over this process's stdin and stdout until the
/// client closes its end. Must run inside a tokio runtime.
pub async fn serve_mcp(root: &Path) {
    let rt = tokio::runtime::Handle::current();
    let scan_root = root.to_path_buf();
    let files = tokio::task::spawn_blocking(move || fs_scan::scan(scan_root).files)
        .await
        .unwrap_or_default();
    let lsp = Arc::new(LspPool::new(root.to_path_buf(), Default::default()));
    let host = Arc::new(CodeTools {
        root: root.to_path_buf(),
        files: Mutex::new((Instant::now(), Arc::new(files))),
        lsp: lsp.clone(),
        rt,
    });
    clew_core::mcp::serve::serve(host, tokio::io::stdin(), tokio::io::stdout()).await;
    lsp.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_comes_from_the_flag_or_the_working_directory() {
        let dir = clew_core::testutil::TempDir::new("mcp-root");
        let named = root_from_args(&[
            "mcp".into(),
            "--root".into(),
            dir.path().to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(named, dir.path().canonicalize().unwrap());
        assert!(root_from_args(&["mcp".into(), "--root".into()]).is_err());
        assert!(root_from_args(&["mcp".into()]).is_ok());
        let missing = dir.path().join("nope");
        assert!(
            root_from_args(&["--root".into(), missing.to_string_lossy().into_owned()]).is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tools_answer_about_the_project() {
        let dir = clew_core::testutil::TempDir::new("mcp-tools");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn add(a: u32) -> u32 {\n    a + 1\n}\n",
        )
        .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let host = CodeTools {
            files: Mutex::new((Instant::now(), Arc::new(fs_scan::scan(root.clone()).files))),
            lsp: Arc::new(LspPool::new(root.clone(), Default::default())),
            rt: tokio::runtime::Handle::current(),
            root,
        };
        let names: Vec<String> = host.tools().into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"search".to_string()) && names.contains(&"read".to_string()));
        assert!(!names.contains(&"answer".to_string()));
        let host = Arc::new(host);
        let h = host.clone();
        let found = tokio::task::spawn_blocking(move || {
            h.call(
                "search",
                &serde_json::json!({"query": "a + 1"}),
                &AtomicBool::new(false),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(found.text.contains("lib.rs:2"), "{}", found.text);
        let h = host.clone();
        let escape = tokio::task::spawn_blocking(move || {
            h.call(
                "read",
                &serde_json::json!({"file": "../../etc/passwd"}),
                &AtomicBool::new(false),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert!(!escape.text.contains("root:"), "confined: {}", escape.text);
        let h = host.clone();
        assert!(
            tokio::task::spawn_blocking(move || h.call(
                "answer",
                &serde_json::json!({}),
                &AtomicBool::new(false)
            ))
            .await
            .unwrap()
            .is_err()
        );
        host.lsp.close();
    }
}
