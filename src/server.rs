//! The in-process clew-server.
//!
//! For now this is a task that speaks the `clew-protocol` over channels to the
//! client (the GUI). It is the seam the client/server split is built on: backend
//! flows migrate onto it one at a time (each becomes a `Request` the server
//! handles and an `Event` it emits), and once the surface is covered it will be
//! extracted into a `clew-server` crate that also runs remotely over SSH — with
//! the client unchanged, because the client only ever talks this protocol.

use std::path::PathBuf;
use std::sync::Arc;

use clew_protocol::{ClientMessage, Event, PROTOCOL_VERSION, Request, ServerMessage};
use iced::futures::{SinkExt, Stream};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::fs_scan::FileEntry;
use crate::{search, Message};

/// The subscription that starts the in-process server and streams its events to
/// the client. On start it hands the client a request sender via
/// `Message::ServerConnected`, then runs the server loop until the app exits.
pub fn subscription() -> iced::Subscription<Message> {
    iced::Subscription::run(stream)
}

/// Plain `fn` (no captures) as `Subscription::run` requires.
fn stream() -> impl Stream<Item = Message> {
    iced::stream::channel(256, |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
        // Hand the client the request end; if the app is already gone, stop.
        if output.send(Message::ServerConnected(tx)).await.is_err() {
            return;
        }
        run(rx, output).await;
    })
}

/// Server-side state. Grows as each backend flow migrates onto the protocol;
/// today it owns the scanned project so it can answer text searches.
#[derive(Default)]
struct Server {
    /// Flat file list from the last `OpenProject` scan — what search greps over.
    files: Option<Arc<Vec<FileEntry>>>,
}

/// The server loop: handle each request and emit its reply, until the client's
/// request channel closes (app shutting down).
async fn run(
    mut rx: UnboundedReceiver<ClientMessage>,
    mut out: iced::futures::channel::mpsc::Sender<Message>,
) {
    let mut server = Server::default();
    while let Some(ClientMessage { id, request }) = rx.recv().await {
        if let Some(event) = server.handle(request).await {
            let msg = Message::ServerEvent(ServerMessage::Reply { id, sub: None, event });
            if out.send(msg).await.is_err() {
                break; // client gone
            }
        }
    }
}

impl Server {
    async fn handle(&mut self, request: Request) -> Option<Event> {
        match request {
            // Handshake: confirm the protocol version.
            Request::Hello { .. } => Some(Event::Ready { protocol: PROTOCOL_VERSION }),
            // Scan the project so later searches have a file list. The client
            // still builds its own tree today, so we don't emit `Tree` yet — that
            // flow migrates later; for now this only feeds search.
            Request::OpenProject { root } => {
                let root = PathBuf::from(root);
                let scan = tokio::task::spawn_blocking(move || crate::fs_scan::scan(root))
                    .await
                    .ok()?;
                self.files = Some(Arc::new(scan.files));
                None
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
