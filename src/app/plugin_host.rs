//! What a plugin may ask of the open project (`clew_plugin::runtime::Host`),
//! answered here for a local project and through the server for a remote
//! one, as the rest of the window reads them (`App::local_project_state`).
//!
//! A plugin runs on a blocking thread, so the remote answers, which are
//! asynchronous, are waited for with the runtime's handle. Every path a
//! plugin names has passed `confine::check_lexical` in the runtime already;
//! a local read confines it again against the root
//! (`read_confined_capped_checked`), as every read of the window's does.

use std::path::PathBuf;
use std::sync::Arc;

use clew_core::fs_scan::FileEntry;
use clew_plugin::runtime::{BlameLine, Commit, Hit, Host, Level, ProjectInfo, Query, Symbol};

use crate::app::rpc::AiClient;

/// The most of a file a plugin may read.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Where the project is read from.
enum Source {
    Local(PathBuf),
    Remote(AiClient, tokio::runtime::Handle),
}

/// The open project, as plugins see it.
pub(crate) struct ProjectHost {
    source: Source,
    name: String,
    files: Arc<Vec<FileEntry>>,
    partial: bool,
    /// What the plugin logs and how far it has come, for the window.
    events: std::sync::Mutex<Vec<HostEvent>>,
}

/// What a plugin said while it ran, for the window to show once it is done.
#[derive(Debug, Clone)]
pub(crate) enum HostEvent {
    Log(Level, String),
}

impl ProjectHost {
    /// A host over a local project at `root`.
    pub(crate) fn local(
        root: PathBuf,
        name: String,
        files: Arc<Vec<FileEntry>>,
        partial: bool,
    ) -> Self {
        ProjectHost {
            source: Source::Local(root),
            name,
            files,
            partial,
            events: Default::default(),
        }
    }

    /// A host over a remote project, read through `ai`; remote answers are
    /// waited for on `handle`.
    pub(crate) fn remote(
        ai: AiClient,
        handle: tokio::runtime::Handle,
        name: String,
        files: Arc<Vec<FileEntry>>,
        partial: bool,
    ) -> Self {
        ProjectHost {
            source: Source::Remote(ai, handle),
            name,
            files,
            partial,
            events: Default::default(),
        }
    }

    /// What the plugin said while it ran.
    pub(crate) fn take_events(&self) -> Vec<HostEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn entry(&self, rel: &str) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.rel == rel)
    }

    /// Wait for `future` on the remote runtime.
    fn wait<T>(handle: &tokio::runtime::Handle, future: impl std::future::Future<Output = T>) -> T {
        handle.block_on(future)
    }
}

impl Host for ProjectHost {
    fn info(&self) -> ProjectInfo {
        ProjectInfo {
            name: self.name.clone(),
            files: self.files.len().min(u32::MAX as usize) as u32,
            partial: self.partial,
            remote: matches!(self.source, Source::Remote(..)),
        }
    }

    fn files(&self) -> Vec<String> {
        self.files.iter().map(|f| f.rel.clone()).collect()
    }

    fn read(&self, path: &str) -> Result<String, String> {
        let entry = self
            .entry(path)
            .ok_or_else(|| format!("{path} is not a file of the project"))?;
        match &self.source {
            Source::Local(root) => {
                match clew_core::fs_scan::read_confined_capped_checked(
                    root,
                    &entry.abs,
                    MAX_READ_BYTES,
                ) {
                    Ok(Some(text)) => Ok(text),
                    Ok(None) => Err(format!("{path} is not text, or larger than 4 MB")),
                    Err(e) => Err(format!("{path}: {e}")),
                }
            }
            Source::Remote(ai, handle) => {
                let rels = vec![path.to_string()];
                let read = Self::wait(handle, async {
                    let ask = |rels| ai.request(clew_protocol::Request::ReadSources { rels });
                    crate::sources::read(rels, ask).await
                });
                read.files
                    .into_iter()
                    .find(|(rel, _)| rel == path)
                    .map(|(_, text)| text)
                    .ok_or_else(|| match read.unread.into_iter().next() {
                        Some((_, Some(why))) => format!("{path}: {why}"),
                        _ => format!("could not read {path} from the remote host"),
                    })
            }
        }
    }

    fn outline(&self, path: &str) -> Result<Vec<Symbol>, String> {
        let text = self.read(path)?;
        let lang = crate::highlight::detect(std::path::Path::new(path))
            .ok_or_else(|| format!("Clew has no outline for {path}"))?;
        Ok(clew_core::outline::extract(&text, lang)
            .into_iter()
            .map(|s| Symbol {
                name: s.name,
                kind: s.kind,
                line: s.line.min(u32::MAX as usize) as u32,
                end_line: s.end_line.min(u32::MAX as usize) as u32,
            })
            .collect())
    }

    fn search(&self, query: &Query) -> Result<Vec<Hit>, String> {
        let include = query.include.join(" ");
        let exclude = query.exclude.join(" ");
        let (hits, error) = match &self.source {
            Source::Local(root) => {
                let report = clew_core::search::search_report(
                    root,
                    self.files.clone(),
                    clew_core::search::SearchOptions {
                        query: query.query.clone(),
                        regex: query.regex,
                        case_sensitive: query.case_sensitive,
                        whole_word: query.whole_word,
                        include,
                        exclude,
                    },
                );
                let hits = report
                    .hits
                    .into_iter()
                    .map(|h| (h.rel, h.line, h.preview))
                    .collect::<Vec<_>>();
                (hits, report.error)
            }
            Source::Remote(ai, handle) => {
                let request = clew_protocol::Request::Search {
                    query: query.query.clone(),
                    regex: query.regex,
                    case_sensitive: query.case_sensitive,
                    whole_word: query.whole_word,
                    include,
                    exclude,
                };
                match Self::wait(handle, ai.request(request))? {
                    clew_protocol::Event::SearchResults { hits, error, .. } => (
                        hits.into_iter()
                            .map(|h| (h.rel.to_string(), h.line, h.preview))
                            .collect::<Vec<_>>(),
                        error,
                    ),
                    _ => return Err("the search was not answered".to_string()),
                }
            }
        };
        if let Some(error) = error {
            return Err(error);
        }
        Ok(hits
            .into_iter()
            .map(|(path, line, text)| Hit {
                path,
                line: line.min(u32::MAX as usize) as u32,
                text,
            })
            .collect())
    }

    fn git_log(&self, path: Option<&str>, limit: u32) -> Result<Vec<Commit>, String> {
        let op = clew_protocol::GitOp::Log {
            all: false,
            rel: path.map(String::from),
            author: None,
            grep: None,
            skip: 0,
            limit: limit.max(1) as usize,
        };
        let result = match &self.source {
            Source::Local(root) => clew_core::git::run_op(root, op).map_err(|e| e.to_string())?,
            Source::Remote(ai, handle) => Self::wait(handle, ai.git(op))?,
        };
        let clew_protocol::GitResult::Log(page) = result else {
            return Err("git answered something else".to_string());
        };
        Ok(page
            .commits
            .into_iter()
            .map(|c| Commit {
                id: c.sha,
                author: c.author,
                time: c.time,
                subject: c.subject,
            })
            .collect())
    }

    fn git_blame(&self, path: &str) -> Result<Vec<BlameLine>, String> {
        let entry = self
            .entry(path)
            .ok_or_else(|| format!("{path} is not a file of the project"))?;
        let info = match &self.source {
            Source::Local(root) => {
                clew_core::git::try_info(root, &entry.abs).map_err(|e| e.to_string())?
            }
            Source::Remote(ai, handle) => {
                let request = clew_protocol::Request::GitInfo {
                    rel: path.to_string(),
                };
                match Self::wait(handle, ai.request(request))? {
                    clew_protocol::Event::GitInfo { info, .. } => info,
                    _ => return Err("git answered something else".to_string()),
                }
            }
        };
        let info = info.ok_or_else(|| "the project has no git history".to_string())?;
        Ok(info
            .blame
            .into_iter()
            .enumerate()
            .map(|(i, b)| BlameLine {
                line: (i + 1).min(u32::MAX as usize) as u32,
                author: b.author,
                time: b.time,
                commit: b.commit,
            })
            .collect())
    }

    fn log(&self, level: Level, message: &str) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(HostEvent::Log(level, message.to_string()));
    }
}
