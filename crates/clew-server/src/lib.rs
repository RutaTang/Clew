//! clew-server: the headless backend.
//!
//! It owns all filesystem / OS interaction and answers the client over
//! `clew-protocol`. The same `Server` logic runs whether the transport is a
//! local child process (stdio) or an SSH session to a remote host — the client
//! only ever speaks the protocol, so local and remote are indistinguishable to
//! it. Backend flows migrate onto `Server::handle` one at a time; today it
//! scans a project and answers text searches.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clew_core::fs_scan::FileEntry;
use clew_core::{docs, embed, git, highlight, inactive, llm, outline, search};
use clew_protocol::{ClientMessage, Event, PROTOCOL_VERSION, Request, ServerMessage};
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

/// A subprocess spawned for the client (a language server / debug adapter): its
/// stdin to write to and the child handle to keep alive and later kill.
struct Proc {
    stdin: tokio::process::ChildStdin,
    child: tokio::process::Child,
}

/// Debounce window: coalesces the burst a single save or `git pull` produces.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// The concrete debouncer type, held to keep the watch thread alive.
type Watcher = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

/// Backend state. Grows as each flow migrates onto the protocol; today it owns
/// the scanned project (for search/read) and watches it for changes.
pub struct Server {
    /// Root of the currently open project; `rel` paths resolve against it.
    root: Option<PathBuf>,
    /// Flat file list from the last `OpenProject` scan — what search greps over.
    files: Option<Arc<Vec<FileEntry>>>,
    /// Channel to push replies and unsolicited notifications (e.g. file changes).
    out: UnboundedSender<ServerMessage>,
    /// Live filesystem watcher for the open project; held to keep it running.
    _watcher: Option<Watcher>,
    /// Subprocesses spawned for the client (language servers, debug adapters),
    /// keyed by the client-assigned handle.
    procs: HashMap<u64, Proc>,
    /// AI provider config to use when the server makes calls (endpoint = Server).
    ai_chat: Option<llm::Config>,
    ai_embed: Option<embed::Config>,
}

impl Server {
    /// Create a server that emits messages on `out`.
    pub fn new(out: UnboundedSender<ServerMessage>) -> Self {
        Server {
            root: None,
            files: None,
            out,
            _watcher: None,
            procs: HashMap::new(),
            ai_chat: None,
            ai_embed: None,
        }
    }

    /// Handle one request, returning the event to reply with (or `None` when a
    /// request has no direct reply).
    pub async fn handle(&mut self, request: Request) -> Option<Event> {
        match request {
            // Handshake: confirm the protocol version.
            Request::Hello { .. } => Some(Event::Ready { protocol: PROTOCOL_VERSION }),
            // Scan the project: store the file list for search/read, and reply
            // with the tree so the client renders it instead of scanning itself.
            Request::OpenProject { root } => {
                let root = PathBuf::from(root);
                self.root = Some(root.clone());
                let scan_root = root.clone();
                let scan = tokio::task::spawn_blocking(move || clew_core::fs_scan::scan(scan_root))
                    .await
                    .ok()?;
                let files: Vec<String> = scan.files.iter().map(|f| f.rel.clone()).collect();
                self.files = Some(Arc::new(scan.files));
                // Start watching the project; changes stream back as notifications.
                self._watcher = spawn_watcher(root, self.out.clone());
                Some(Event::Tree {
                    tree: scan.tree,
                    files,
                    truncated: scan.truncated,
                })
            }
            // Read + tokenize a file for display. `rel` resolves against the
            // project root; the reply carries per-line (text, style-index) spans
            // that the client maps to theme colors.
            Request::ReadFile { rel, target } => {
                let root = self.root.clone()?;
                // Confine the read to the project. `rel` comes from the client
                // (untrusted, especially over SSH), so reject anything that
                // escapes root — absolute paths, `..`, or symlinks pointing out.
                let Some(abs) = confine(&root, &rel) else {
                    return Some(Event::Error {
                        message: format!("refused: path escapes project: {rel}"),
                    });
                };
                let target: inactive::Target = target.into();
                let read = tokio::task::spawn_blocking(move || {
                    let source = std::fs::read_to_string(&abs)?;
                    let lang = highlight::detect(&abs);
                    let lines = highlight::highlight_lines(&source, lang);
                    // Symbols, doc comments, and inactive #[cfg] lines — the rest
                    // of what a file view shows, computed from the same read.
                    let (symbols, docs, inactive) = match lang {
                        Some(key) => {
                            let symbols = outline::extract(&source, key);
                            let docs = docs::extract(&source, key, &symbols);
                            let inactive = inactive::inactive_lines(&source, key, &target);
                            (symbols, docs, inactive)
                        }
                        None => Default::default(),
                    };
                    Ok::<_, std::io::Error>((source, lines, symbols, docs, inactive))
                })
                .await;
                match read {
                    Ok(Ok((source, lines, symbols, docs, inactive))) => Some(Event::FileContent {
                        rel,
                        source,
                        lines,
                        symbols,
                        docs: docs.into_iter().collect(),
                        inactive: inactive.into_iter().collect(),
                    }),
                    Ok(Err(e)) => Some(Event::Error {
                        message: format!("read {rel}: {e}"),
                    }),
                    Err(_) => None, // task join failed
                }
            }
            // Per-file git blame + change status for the gutter. Confined to the
            // project like ReadFile; `None` when the file is untracked.
            Request::GitInfo { rel } => {
                let root = self.root.clone()?;
                let Some(abs) = confine(&root, &rel) else {
                    return Some(Event::Error {
                        message: format!("refused: path escapes project: {rel}"),
                    });
                };
                let groot = root.clone();
                let info = tokio::task::spawn_blocking(move || git::info(&groot, &abs))
                    .await
                    .ok()
                    .flatten();
                Some(Event::GitInfo { rel, info })
            }
            // Grep the scanned project. Reuses the same search engine the client
            // used to run in-process; only where it runs has changed.
            Request::Search {
                query,
                regex,
                case_sensitive,
                whole_word,
                include,
                exclude,
            } => {
                let files = self.files.clone()?;
                let opts = search::SearchOptions {
                    query,
                    regex,
                    case_sensitive,
                    whole_word,
                    include,
                    exclude,
                };
                let result = tokio::task::spawn_blocking(move || search::search(files, opts))
                    .await
                    .unwrap_or_default();
                let hits = result
                    .hits
                    .into_iter()
                    .map(|h| clew_protocol::SearchHit {
                        rel: h.rel,
                        line: h.line,
                        preview: h.preview,
                    })
                    .collect();
                Some(Event::SearchResults {
                    hits,
                    error: result.error,
                })
            }
            // Spawn a subprocess and stream its stdout back, so a debug adapter
            // runs where the code lives.
            Request::SpawnProcess {
                proc,
                cmd,
                args,
                cwd,
            } => self.spawn_and_proxy(proc, cmd, args, cwd),
            // Start a language server the server resolves itself — the client
            // never ships a binary path, so a remote uses its own LSP.
            Request::SpawnLsp { proc, language } => {
                let Some(root) = self.root.clone() else { return None };
                let config =
                    clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap_or_default();
                let Some(server) = config.resolve(&language) else {
                    // No server configured: end the proxy so the client sees EOF.
                    self.notify_proc_exited(proc);
                    return None;
                };
                use clew_core::lsp::store::Located;
                let exe = match server.command.clone() {
                    Some(cmd) => cmd,
                    None => match clew_core::lsp::store::locate(&server) {
                        Located::Ready(exe) => exe,
                        // Not installed on this host: provision it (download +
                        // unpack for the server's own platform).
                        Located::NeedsDownload { download, dest_dir } => {
                            let installed = tokio::task::spawn_blocking(move || {
                                clew_core::lsp::store::download_and_install(&download, &dest_dir)
                            })
                            .await;
                            match installed {
                                Ok(Ok(exe)) => exe,
                                Ok(Err(e)) => {
                                    self.notify_proc_exited(proc);
                                    return Some(Event::Error {
                                        message: format!("install {language} server: {e}"),
                                    });
                                }
                                Err(_) => {
                                    self.notify_proc_exited(proc);
                                    return None;
                                }
                            }
                        }
                        _ => {
                            self.notify_proc_exited(proc);
                            return Some(Event::Error {
                                message: format!("no {language} server for this platform"),
                            });
                        }
                    },
                };
                let cwd = Some(root.to_string_lossy().into_owned());
                self.spawn_and_proxy(proc, exe.to_string_lossy().into_owned(), server.args, cwd)
            }
            Request::ProcessInput { proc, data } => {
                if let Some(p) = self.procs.get_mut(&proc) {
                    let _ = p.stdin.write_all(&data).await;
                    let _ = p.stdin.flush().await;
                }
                None
            }
            Request::ProcessKill { proc } => {
                if let Some(mut p) = self.procs.remove(&proc) {
                    let _ = p.child.start_kill();
                }
                None
            }
            // Store the AI config for server-side calls.
            Request::SetAiConfig { chat, embed } => {
                self.ai_chat = chat.map(|c| llm::Config {
                    provider: llm::Provider::from_slug(&c.provider),
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                });
                self.ai_embed = embed.map(|c| embed::Config {
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                });
                None
            }
            // Run a chat completion with the server's config (blocking HTTP off
            // the reactor). The whole response comes back in one reply.
            Request::Chat {
                system,
                messages,
                max_tokens,
            } => {
                let Some(cfg) = self.ai_chat.clone() else {
                    return Some(Event::Error {
                        message: "no AI chat config on the server".into(),
                    });
                };
                let msgs: Vec<llm::ChatMsg> = messages
                    .into_iter()
                    .map(|m| {
                        if m.role == "assistant" {
                            llm::ChatMsg::assistant(m.content)
                        } else {
                            llm::ChatMsg::user(m.content)
                        }
                    })
                    .collect();
                let result = tokio::task::spawn_blocking(move || {
                    llm::complete_chat(&cfg, &system, &msgs, max_tokens)
                })
                .await;
                match result {
                    Ok(Ok(text)) => Some(Event::ChatResult { text }),
                    Ok(Err(e)) => Some(Event::Error { message: e }),
                    Err(_) => None,
                }
            }
            // Embed texts with the server's embedding config.
            Request::Embed { texts } => {
                let Some(cfg) = self.ai_embed.clone() else {
                    return Some(Event::Error {
                        message: "no embedding config on the server".into(),
                    });
                };
                let result =
                    tokio::task::spawn_blocking(move || embed::embed_all(&cfg, &texts)).await;
                match result {
                    Ok(Ok(vecs)) => Some(Event::Embeddings { vecs }),
                    Ok(Err(e)) => Some(Event::Error { message: e }),
                    Err(_) => None,
                }
            }
            Request::ListDir { path } => Some(list_dir(path).await),
            Request::BuildDocs => {
                let files = self.files.clone()?;
                let built = tokio::task::spawn_blocking(move || build_docs(&files)).await;
                built.ok().map(|files| Event::Docs { files })
            }
            // Remaining flows migrate here (Outline, Explain, …).
            _ => None,
        }
    }

    /// Tell the client a proxied process is gone (so its client-side driver, e.g.
    /// an LspClient, sees EOF and fails cleanly).
    fn notify_proc_exited(&self, proc: u64) {
        let _ = self.out.send(ServerMessage::Notification {
            sub: None,
            event: Event::ProcessExited { proc, code: None },
        });
    }

    /// Spawn `cmd` (in `cwd` or the project root) and proxy its stdio to the
    /// client under handle `proc`: stdout streams back as `ProcessOutput`, and
    /// its stdin is written by `ProcessInput`. Shared by SpawnProcess/SpawnLsp.
    fn spawn_and_proxy(
        &mut self,
        proc: u64,
        cmd: String,
        args: Vec<String>,
        cwd: Option<String>,
    ) -> Option<Event> {
        let dir = cwd.map(PathBuf::from).or_else(|| self.root.clone());
        let mut command = tokio::process::Command::new(&cmd);
        command
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        match command.spawn() {
            Ok(mut child) => {
                let Some(stdin) = child.stdin.take() else {
                    return None;
                };
                let Some(mut stdout) = child.stdout.take() else {
                    return None;
                };
                let out = self.out.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    loop {
                        match stdout.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let msg = ServerMessage::Notification {
                                    sub: None,
                                    event: Event::ProcessOutput {
                                        proc,
                                        data: buf[..n].to_vec(),
                                    },
                                };
                                if out.send(msg).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = out.send(ServerMessage::Notification {
                        sub: None,
                        event: Event::ProcessExited { proc, code: None },
                    });
                });
                self.procs.insert(proc, Proc { stdin, child });
                None
            }
            Err(e) => Some(Event::Error {
                message: format!("spawn {cmd}: {e}"),
            }),
        }
    }
}

/// Build the project's API documentation index: for every file with a
/// recognized language and a non-empty documented API, its nested doc items.
/// Blocking; run off the async runtime.
fn build_docs(files: &[FileEntry]) -> Vec<clew_protocol::DocFile> {
    let mut out = Vec::new();
    for f in files {
        let Some(lang) = highlight::detect(&f.abs) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&f.abs) else {
            continue;
        };
        let items = clew_core::apidoc::build_file(&source, lang);
        if !items.is_empty() {
            out.push(clew_protocol::DocFile {
                rel: f.rel.clone(),
                items,
            });
        }
    }
    out
}

/// List a directory on this host for the remote folder picker. `path` is an
/// absolute or `~`-relative directory, or `None` for the login home. Directories
/// sort before files, each alphabetically (case-insensitive). Unreadable entries
/// are skipped rather than failing the whole listing.
async fn list_dir(path: Option<String>) -> Event {
    let home = std::env::var("HOME").ok();
    // Resolve the target directory: home when unset, `~`-expanded, else as given.
    let dir: PathBuf = match path.as_deref() {
        None | Some("") | Some("~") => match &home {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from("/"),
        },
        Some(p) if p == "~" || p.starts_with("~/") => match &home {
            Some(h) => Path::new(h).join(p.trim_start_matches("~/")),
            None => PathBuf::from(p),
        },
        Some(p) => PathBuf::from(p),
    };
    // Canonicalize so the reported path and its parent are stable and absolute.
    let dir = tokio::fs::canonicalize(&dir).await.unwrap_or(dir);

    let mut read = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => {
            return Event::Error {
                message: format!("cannot list {}: {e}", dir.display()),
            };
        }
    };
    let mut entries: Vec<clew_protocol::DirEntry> = Vec::new();
    while let Ok(Some(ent)) = read.next_entry().await {
        let name = ent.file_name().to_string_lossy().into_owned();
        // A symlink to a directory should still browse as one.
        let is_dir = match ent.file_type().await {
            Ok(ft) if ft.is_symlink() => tokio::fs::metadata(ent.path())
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false),
            Ok(ft) => ft.is_dir(),
            Err(_) => continue,
        };
        entries.push(clew_protocol::DirEntry { name, is_dir });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Event::DirListing {
        path: dir.to_string_lossy().into_owned(),
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        entries,
    }
}

/// Resolve `rel` against `root`, refusing anything that escapes the project:
/// absolute paths, `..` traversal, or symlinks that point outside `root`.
/// Returns the canonical absolute path only when it genuinely lives inside root.
fn confine(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    // Reject absolute paths and any parent/root/prefix components up front.
    let escapes = rel_path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if rel_path.is_absolute() || escapes {
        return None;
    }
    // Canonicalize and confirm containment. Canonicalizing resolves symlinks, so
    // a link inside the project that points outside it is rejected too.
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let canonical = std::fs::canonicalize(canonical_root.join(rel_path)).ok()?;
    canonical.starts_with(&canonical_root).then_some(canonical)
}

/// Watch `root` recursively; stream changes back on `out` as notifications. A
/// content change emits `FilesChanged`; a create/delete also re-scans and emits
/// an updated `Tree`. Returns the debouncer, which must be kept alive to run.
fn spawn_watcher(root: PathBuf, out: UnboundedSender<ServerMessage>) -> Option<Watcher> {
    let cb_root = root.clone();
    let mut debouncer = new_debouncer(
        DEBOUNCE,
        None,
        move |res: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = res else { return };
            let mut rels: Vec<String> = Vec::new();
            let mut structural = false;
            for ev in &events {
                let relevant = matches!(
                    ev.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                );
                if !relevant {
                    continue;
                }
                if matches!(ev.kind, EventKind::Create(_) | EventKind::Remove(_)) {
                    structural = true;
                }
                for p in &ev.paths {
                    if is_noise(p) {
                        continue;
                    }
                    if let Ok(rel) = p.strip_prefix(&cb_root) {
                        rels.push(rel.to_string_lossy().into_owned());
                    }
                }
            }
            // A create/delete changes the file set: re-scan and push a fresh tree.
            if structural {
                let scan = clew_core::fs_scan::scan(cb_root.clone());
                let files = scan.files.iter().map(|f| f.rel.clone()).collect();
                let _ = out.send(ServerMessage::Notification {
                    sub: None,
                    event: Event::Tree {
                        tree: scan.tree,
                        files,
                        truncated: scan.truncated,
                    },
                });
            }
            rels.sort();
            rels.dedup();
            if !rels.is_empty() {
                let _ = out.send(ServerMessage::Notification {
                    sub: None,
                    event: Event::FilesChanged { rels },
                });
            }
        },
    )
    .ok()?;
    debouncer.watch(&root, RecursiveMode::Recursive).ok()?;
    Some(debouncer)
}

/// Skip VCS internals, build output, dependencies, and clew's own data dir so a
/// `cargo build` or `npm install` doesn't drown the channel.
fn is_noise(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git")
                | Some("target")
                | Some("node_modules")
                | Some(".clew")
                | Some(".hg")
                | Some(".svn")
                | Some(".idea")
                | Some(".DS_Store")
        )
    })
}

/// Run the server over stdio until the client's stream ends (or stdin closes).
///
/// Framing is newline-delimited JSON: each `ClientMessage` arrives as one line,
/// each `ServerMessage` is written back as one line. serde_json's compact output
/// never contains a literal newline (string values escape theirs), so a line is
/// always exactly one message. A dedicated writer task drains the output channel
/// so replies and unsolicited notifications (file changes) share one stdout.
pub async fn serve_stdio() {
    let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let Ok(mut json) = serde_json::to_string(&msg) else {
                continue;
            };
            json.push('\n');
            if stdout.write_all(json.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut server = Server::new(out.clone());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            continue;
        }
        let Ok(ClientMessage { id, request }) = serde_json::from_str::<ClientMessage>(&line) else {
            continue; // ignore malformed frames rather than dying
        };
        if let Some(event) = server.handle(request).await {
            if out
                .send(ServerMessage::Reply { id, sub: None, event })
                .is_err()
            {
                break; // writer gone
            }
        }
    }
    drop(server); // stop the watcher
    drop(out); // close the channel so the writer task ends
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::confine;
    use std::path::Path;

    #[test]
    fn confine_allows_files_inside_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(confine(root, "Cargo.toml").is_some());
        assert!(confine(root, "src/lib.rs").is_some());
    }

    #[test]
    fn confine_rejects_escapes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(confine(root, "/etc/passwd").is_none()); // absolute path
        assert!(confine(root, "../clew-core/Cargo.toml").is_none()); // parent escape
        assert!(confine(root, "src/../../Cargo.toml").is_none()); // .. in the middle
        assert!(confine(root, "does/not/exist.rs").is_none()); // nonexistent
    }
}
