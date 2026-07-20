//! clew-server: the headless backend.
//!
//! It owns all filesystem / OS interaction and answers the client over
//! `clew-protocol`. The same `Server` logic runs whether the transport is a
//! local child process (stdio) or an SSH session to a remote host — the client
//! only ever speaks the protocol, so local and remote are indistinguishable to
//! it. Backend flows migrate onto `Server::handle` one at a time; today it
//! scans a project and answers text searches.

use std::path::PathBuf;
use std::sync::Arc;

use clew_core::fs_scan::FileEntry;
use clew_core::{highlight, search};
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
            Request::ReadFile { rel } => {
                let root = self.root.clone()?;
                let abs = root.join(&rel);
                let read = tokio::task::spawn_blocking(move || {
                    std::fs::read_to_string(&abs).map(|source| {
                        let lang = highlight::detect(&abs);
                        highlight::highlight_lines(&source, lang)
                    })
                })
                .await;
                match read {
                    Ok(Ok(lines)) => Some(Event::FileContent { rel, lines }),
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
