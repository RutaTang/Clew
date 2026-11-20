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
//! It also serves the plugin tools (`clew_plugin::tools`): an agent writes
//! a plugin's source and builds it here, and asks Clew's window, which has
//! the plugin runtime and the user, to try a build on the open project or
//! to ask the user to install it (`clew_plugin::requests`).
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

/// How long a request may wait for a Clew window to take it before the
/// agent is told no window is open for it.
const TAKE_WITHIN: Duration = Duration::from_secs(8);

/// How long a try waits for Clew's window: a command is stopped after 30 s,
/// a graph after 60 s.
const TRY_WAIT: Duration = Duration::from_secs(90);

/// How long an install waits for the user to answer.
const INSTALL_WAIT: Duration = Duration::from_secs(15 * 60);

/// The project the server serves, and what its tools share.
struct CodeTools {
    root: PathBuf,
    files: Mutex<(Instant, Arc<Vec<FileEntry>>)>,
    lsp: Arc<LspPool>,
    rt: tokio::runtime::Handle,
    /// The client's name, for the user to see who asked for an install.
    client: Mutex<String>,
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
            clew_version().into(),
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
                read_only: true,
            })
            .chain(
                clew_plugin::tools::tool_defs(clew_plugin::tools::Caller::Outside)
                    .into_iter()
                    .map(|t| Tool {
                        name: t.name.into(),
                        title: None,
                        description: format!("[Clew plugins] {}", t.description),
                        input_schema: t.schema,
                        read_only: t.read_only,
                    }),
            )
            .collect()
    }

    fn client(&self, name: &str) {
        *self.client.lock().unwrap_or_else(PoisonError::into_inner) =
            name.chars().filter(|c| !c.is_control()).take(80).collect();
    }

    fn call(
        &self,
        name: &str,
        args: &serde_json::Value,
        stop: &AtomicBool,
    ) -> Result<ToolOutput, String> {
        if clew_plugin::tools::is_plugin_tool(name) {
            let (text, is_error) = match self.plugin_call(name, args, stop) {
                Ok(text) => (text, false),
                Err(why) => (why, true),
            };
            return Ok(ToolOutput { text, is_error });
        }
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

impl CodeTools {
    /// A plugin tool: done here, or asked of Clew's window.
    fn plugin_call(
        &self,
        name: &str,
        args: &serde_json::Value,
        stop: &AtomicBool,
    ) -> Result<String, String> {
        use clew_plugin::requests::{Answer, Ask, Waited};
        use clew_plugin::tools::Step;
        let data =
            clew_core::lsp::store::data_root().ok_or("Clew has no data folder on this machine")?;
        let agent = {
            let client = self.client.lock().unwrap_or_else(PoisonError::into_inner);
            // Unnamed, the window says "an agent" in the user's language.
            client.clone()
        };
        // What an install says once the user allowed it: by the plugin's
        // manifest, its commands named as the window's palette names them.
        let mut installed = None;
        let (ask, wait) = match clew_plugin::tools::call(&data, name, args, stop) {
            Step::Done(result) => return result,
            Step::Try {
                built,
                command,
                graph,
                file,
                line,
                input,
            } => (
                Ask::Try {
                    plugin: built.inspected.manifest.plugin.id.clone(),
                    sha256: clew_plugin::inspect::hex(&built.inspected.sha256),
                    command,
                    graph,
                    root: self.root.to_string_lossy().into_owned(),
                    file,
                    line,
                    input,
                },
                TRY_WAIT,
            ),
            Step::Install { built } => {
                let manifest = &built.inspected.manifest;
                installed = Some(manifest.clone());
                (
                    Ask::Install {
                        plugin: manifest.plugin.id.clone(),
                        sha256: clew_plugin::inspect::hex(&built.inspected.sha256),
                    },
                    INSTALL_WAIT,
                )
            }
        };
        let installing = matches!(ask, Ask::Install { .. });
        let nonce = clew_plugin::requests::file(&data, ask, &agent)?;
        // A window takes a request within a second of it being filed: one
        // not taken in a few means none is open for it, said now rather
        // than after the whole wait.
        match clew_plugin::requests::wait_taken(&data, &nonce, TAKE_WITHIN, wait, stop) {
            Waited::Answered(Answer::Tried { result }) => Ok(result),
            Waited::Answered(Answer::Installed { palette, .. }) => Ok(installed
                .map(|m| m.installed_words_as(&palette))
                .unwrap_or_default()),
            Waited::Answered(Answer::Declined) => Ok("The user chose not to install it.".into()),
            Waited::Answered(Answer::Failed { why }) => Err(why),
            Waited::NotTaken if installing => Err(
                "Clew is not open, so the user could not be asked. Ask them to open Clew, \
                 then try again."
                    .into(),
            ),
            Waited::NotTaken => Err(format!(
                "No Clew window has this project open, so the try was not run. Ask the user \
                 to open {} in Clew, then try again.",
                self.root.display()
            )),
            Waited::NoAnswer if installing => Err(
                "The user did not answer in Clew, so the install was withdrawn. Ask them \
                 whether they want it, then try again."
                    .into(),
            ),
            Waited::NoAnswer => Err("The try did not finish in time.".into()),
        }
    }
}

/// Clew's release version, as its workspace's root package gives it: this
/// crate's own version is an internal 0.1.0, which an agent took for
/// Clew's.
fn clew_version() -> &'static str {
    package_version(include_str!("../../../Cargo.toml")).unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The `version` of a `Cargo.toml`'s `[package]`.
fn package_version(manifest: &str) -> Option<&str> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return value.trim().strip_prefix('"')?.split('"').next();
        }
    }
    None
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
        client: Mutex::new(String::new()),
    });
    clew_core::mcp::serve::serve(host, tokio::io::stdin(), tokio::io::stdout()).await;
    lsp.close();
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_server_says_clews_release_version() {
        let manifest = "[workspace]\nmembers = []\n\n[package]\nname = \"clew\"\nversion = \"0.2.5\"\n\n[dependencies]\nx = { version = \"1\" }\n";
        assert_eq!(package_version(manifest), Some("0.2.5"));
        assert_eq!(package_version("[dependencies]\nversion = \"9\"\n"), None);
        let real = clew_version();
        assert!(real.split('.').count() == 3, "{real}");
        assert_ne!(real, "0.1.0", "the root package's, not this crate's");
    }

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
            client: Mutex::new(String::new()),
        };
        let names: Vec<String> = host.tools().into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"search".to_string()) && names.contains(&"read".to_string()));
        assert!(!names.contains(&"answer".to_string()));
        // The plugin tools are served beside the code tools; those that do
        // more than read say so, for the client to ask before them.
        let tools = host.tools();
        let read_only = |name: &str| tools.iter().find(|t| t.name == name).unwrap().read_only;
        assert!(read_only("search") && read_only("plugin_guide"));
        assert!(!read_only("plugin_write") && !read_only("plugin_install"));
        for plugin_tool in [
            "plugin_guide",
            "plugin_new",
            "plugin_build",
            "plugin_install",
        ] {
            assert!(names.contains(&plugin_tool.to_string()), "{plugin_tool}");
        }
        host.client("Claude Code\n");
        assert_eq!(*host.client.lock().unwrap(), "Claude Code");
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
