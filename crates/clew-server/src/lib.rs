//! clew-server: the headless backend.
//!
//! It owns all filesystem / OS interaction and answers the client over
//! `clew-protocol`. The same `Server` logic runs whether the transport is a
//! local child process (stdio) or an SSH session to a remote host — the client
//! only ever speaks the protocol, so local and remote are indistinguishable to
//! it. Backend flows migrate onto `Server::handle` one at a time; today it
//! scans a project and answers text searches.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use clew_core::fs_scan::FileEntry;
use clew_core::{docs, highlight, inactive, outline, search};
use clew_protocol::{ClientMessage, Event, PROTOCOL_VERSION, Request, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Backend state. Grows as each flow migrates onto the protocol; today it owns
/// the scanned project so it can answer text searches.
#[derive(Default)]
pub struct Server {
    /// Root of the currently open project; `rel` paths resolve against it.
    root: Option<PathBuf>,
    /// Flat file list from the last `OpenProject` scan — what search greps over.
    files: Option<Arc<Vec<FileEntry>>>,
}

impl Server {
    /// Handle one request, returning the event to reply with (or `None` when a
    /// request has no direct reply).
    pub async fn handle(&mut self, request: Request) -> Option<Event> {
        match request {
            // Handshake: confirm the protocol version.
            Request::Hello { .. } => Some(Event::Ready { protocol: PROTOCOL_VERSION }),
            // Scan the project so later searches have a file list. The client
            // still builds its own tree today, so we don't emit `Tree` yet —
            // that flow migrates later; for now this only feeds search.
            Request::OpenProject { root } => {
                let root = PathBuf::from(root);
                self.root = Some(root.clone());
                let scan = tokio::task::spawn_blocking(move || clew_core::fs_scan::scan(root))
                    .await
                    .ok()?;
                self.files = Some(Arc::new(scan.files));
                None
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
                    Ok::<_, std::io::Error>((lines, symbols, docs, inactive))
                })
                .await;
                match read {
                    Ok(Ok((lines, symbols, docs, inactive))) => Some(Event::FileContent {
                        rel,
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
            // Remaining flows migrate here (ReadFile, Outline, GitInfo, …).
            _ => None,
        }
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

/// Run the server over stdio until the client's stream ends (or stdin closes).
///
/// Framing is newline-delimited JSON: each `ClientMessage` arrives as one line,
/// each `ServerMessage` is written back as one line. serde_json's compact output
/// never contains a literal newline (string values escape theirs), so a line is
/// always exactly one message.
pub async fn serve_stdio() {
    let mut server = Server::default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            continue;
        }
        let Ok(ClientMessage { id, request }) = serde_json::from_str::<ClientMessage>(&line) else {
            continue; // ignore malformed frames rather than dying
        };
        if let Some(event) = server.handle(request).await {
            let reply = ServerMessage::Reply { id, sub: None, event };
            let Ok(mut json) = serde_json::to_string(&reply) else {
                continue;
            };
            json.push('\n');
            if stdout.write_all(json.as_bytes()).await.is_err() {
                break; // client gone
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    }
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
