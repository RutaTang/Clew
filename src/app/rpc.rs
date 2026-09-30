//! The request/reply client every AI and git flow goes through: correlated
//! round trips to the clew-server (with the not-ready retry and timeouts) or
//! the provider / git on this machine, and the fire-and-forget request helper.

use crate::app::prelude::*;
use crate::*;

/// Which side makes this window's outbound AI calls (chat + embeddings): the
/// connected clew-server, or this client.
///
/// A client-side routing choice, and deliberately not a wire field: it can
/// change during a connection (a revoked per-host grant), and the server only
/// ever needs its consequence — whether `SetAiConfig` handed it keys. (It used
/// to ride in `Hello`, where the server ignored it.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiEndpoint {
    /// The server calls the AI provider directly.
    Server,
    /// This client calls the provider itself; no key crosses to the server.
    Client,
}

/// One request's reply, as the transport delivered it (an `Error` included).
pub(crate) type PendingReply = tokio::sync::oneshot::Sender<clew_protocol::Event>;

/// The requests awaiting their replies, by id.
pub(crate) type PendingReplies = std::sync::Mutex<HashMap<u64, PendingReply>>;

/// Lock the pending-reply map, taking it back from a panic in some other
/// holder: the map is a plain id → sender table that no partial update can
/// corrupt, and a poisoned lock must not take down the UI thread that routes
/// every reply (`App::handle_server_reply`) or the next request.
pub(crate) fn lock_replies(
    pending: &PendingReplies,
) -> std::sync::MutexGuard<'_, HashMap<u64, PendingReply>> {
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why a server round trip produced no answer.
#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    /// The server's own classification of a refusal or failure; `None` when
    /// no reply came at all (the transport died, the request timed out).
    pub(crate) code: Option<clew_protocol::ErrorCode>,
    pub(crate) message: String,
}

impl RpcError {
    fn local(message: impl Into<String>) -> RpcError {
        RpcError {
            code: None,
            message: message.into(),
        }
    }
}

impl From<RpcError> for String {
    fn from(e: RpcError) -> String {
        e.message
    }
}

/// Why a completion produced no text, as the side that made the call knows
/// it: what a caller that acts on the failure — the explain pass says what
/// failed, and stops or not by it — classifies, rather than the text.
#[derive(Debug, Clone)]
pub(crate) enum CallError {
    /// This client called the provider, and this is why that failed.
    Llm(llm::LlmError),
    /// The clew-server was to call it: its refusal or failure (`code`), or
    /// no answer at all.
    Rpc(RpcError),
    /// clew failed on this side: a task that panicked, a reply of another
    /// kind than the one asked for.
    Internal(String),
}

impl From<CallError> for String {
    fn from(e: CallError) -> String {
        match e {
            CallError::Llm(e) => e.to_string(),
            CallError::Rpc(e) => e.message,
            CallError::Internal(message) => message,
        }
    }
}

impl CallError {
    /// The HTTP status the provider answered the call with, where it
    /// answered with one — here, or to the clew-server that made the call.
    pub(crate) fn status(&self) -> Option<u16> {
        use clew_protocol::{ErrorCode, ProviderFailure};
        match self {
            CallError::Llm(llm::LlmError::Status { code, .. })
            | CallError::Rpc(RpcError {
                code: Some(ErrorCode::Provider(ProviderFailure::Status { code, .. })),
                ..
            }) => Some(*code),
            _ => None,
        }
    }
}

/// Routes AI calls to clew-server (endpoint = Server) or runs them locally
/// (endpoint = Client). Cheap to clone (handles only), so each background AI
/// task takes one.
#[derive(Clone)]
pub struct AiClient {
    pub(crate) endpoint: AiEndpoint,
    pub(crate) server_tx: Option<server::RequestTx>,
    pub(crate) next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) pending:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u64, PendingReply>>>,
}

impl AiClient {
    /// The request channel to use when the AI endpoint is the server.
    fn server(&self) -> Option<&server::RequestTx> {
        if self.endpoint == AiEndpoint::Server {
            self.server_tx.as_ref()
        } else {
            None
        }
    }

    /// Send any protocol request and await its correlated reply — the
    /// generic RPC the AI helpers are built on, also used for other server
    /// round-trips (remote launch-config fetch, debug-adapter spawn, stats,
    /// the call graph). Ignores the AI endpoint choice: these are protocol
    /// operations, not AI calls. A server `Error` is an `Err` with its message.
    pub(crate) async fn request(
        &self,
        request: clew_protocol::Request,
    ) -> Result<clew_protocol::Event, String> {
        let tx = self
            .server_tx
            .as_ref()
            .ok_or("not connected to a server".to_string())?;
        Ok(self.rpc(tx, request).await?)
    }

    /// Run one git operation on the server (where a remote repository lives).
    /// Every failure — the transport, the server's refusal, a reply that is
    /// not a `GitResult` — is an `Err`, never an empty value: an empty
    /// history, an empty diff or a missing commit message is a real answer
    /// that callers act on (and feed to paid prompts), so a failure must not
    /// be able to pass for one.
    pub(crate) async fn git(
        &self,
        op: clew_protocol::GitOp,
    ) -> Result<clew_protocol::GitResult, String> {
        match self.request(clew_protocol::Request::Git { op }).await? {
            clew_protocol::Event::GitResult { result, .. } => Ok(result),
            other => Err(format!(
                "unexpected reply to a git request: {}",
                event_name(&other)
            )),
        }
    }

    /// Send a request and await its correlated reply (resolved in `update`).
    /// Retries (bounded) on the server's not-ready refusal
    /// ([`clew_protocol::ErrorCode::NotReady`], the `OpenProject` scan window)
    /// for idempotent read requests only; a non-idempotent request must never
    /// be silently re-sent.
    ///
    /// The list names exactly the requests that come THROUGH here and that
    /// the server can refuse as not ready — the reads that need the finished
    /// scan (see `clew_server`'s `not_ready`): `Stats` and `ProjectCalls`.
    /// Every other read answers without waiting for the scan, so listing it
    /// retried nothing. The requests a window correlates itself — a Search, a
    /// `BuildDocs` — are re-sent by `App::send_retrying` instead.
    async fn rpc(
        &self,
        tx: &server::RequestTx,
        request: clew_protocol::Request,
    ) -> Result<clew_protocol::Event, RpcError> {
        use clew_protocol::Request;
        let idempotent = matches!(request, Request::Stats | Request::ProjectCalls { .. });
        let mut delay = std::time::Duration::from_millis(250);
        for _ in 0..4 {
            match self.rpc_once(tx, request.clone()).await {
                Err(RpcError {
                    code: Some(clew_protocol::ErrorCode::NotReady),
                    ..
                }) if idempotent => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            }
        }
        self.rpc_once(tx, request).await
    }

    /// One send + correlated await, with the bookkeeping kept leak-free
    /// however the wait ends — a reply, a failed send, the timeout, or the
    /// waiting future being dropped (its task aborted: a cancelled explain
    /// pass, a project switch). That last one is what [`Awaiting`] is for: it
    /// removes the pending entry, and for a request the server can stop (a
    /// `Chat`, an `Embed`) it sends `Cancel { id }`, so an abandoned
    /// completion, or index build, stops running — and billing — on the
    /// server instead of for up to ten minutes with nobody left to read it.
    /// The timeout is generous for AI calls (a big completion takes minutes)
    /// and tight for everything else; without one, a reply that never comes
    /// (a lost frame, a server bug) parked the caller and its pending entry
    /// forever.
    async fn rpc_once(
        &self,
        tx: &server::RequestTx,
        request: clew_protocol::Request,
    ) -> Result<clew_protocol::Event, RpcError> {
        use clew_protocol::Request;
        let ai = matches!(request, Request::Chat { .. } | Request::Embed { .. });
        let limit = if ai {
            std::time::Duration::from_secs(600)
        } else {
            std::time::Duration::from_secs(60)
        };
        // The AI calls are the requests the server can stop.
        let cancellable = ai;
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + limit;
        let (otx, orx) = tokio::sync::oneshot::channel();
        lock_replies(&self.pending).insert(id, otx);
        let mut awaiting = Awaiting {
            id,
            pending: &self.pending,
            cancel: cancellable.then_some((tx, &*self.next_id)),
            sent: false,
        };
        // A background task, so a full request queue is waited out
        // (backpressure) — within the same time limit as the reply, so a
        // server that stopped reading fails the call instead of parking it.
        let queued = tokio::time::timeout_at(
            deadline,
            tx.send(clew_protocol::ClientMessage { id, request }),
        )
        .await;
        match queued {
            Ok(Ok(())) => awaiting.sent = true,
            Ok(Err(_)) => return Err(RpcError::local("server gone")),
            Err(_) => {
                return Err(RpcError::local(format!(
                    "the server took no requests for {}s",
                    limit.as_secs()
                )));
            }
        }
        match tokio::time::timeout_at(deadline, orx).await {
            Ok(Ok(clew_protocol::Event::Error { code, message })) => Err(RpcError {
                code: Some(code),
                message,
            }),
            Ok(Ok(event)) => Ok(event),
            Ok(Err(_)) => Err(RpcError::local("server dropped the request")),
            // Given up on: `awaiting` forgets the entry and, for a `Chat`,
            // stops the server's work that nobody will read now.
            Err(_) => Err(RpcError::local(format!(
                "no reply from the server within {}s",
                limit.as_secs()
            ))),
        }
    }

    /// A single-prompt completion.
    pub async fn complete(
        &self,
        cfg: llm::Config,
        system: &str,
        prompt: String,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.complete_chat(cfg, system, vec![llm::ChatMsg::user(prompt)], max_tokens)
            .await
    }

    /// [`complete`](AiClient::complete), failing with why, typed
    /// ([`CallError`]).
    pub(crate) async fn complete_typed(
        &self,
        cfg: llm::Config,
        system: &str,
        prompt: String,
        max_tokens: u32,
    ) -> Result<String, CallError> {
        self.chat_typed(cfg, system, vec![llm::ChatMsg::user(prompt)], max_tokens)
            .await
    }

    /// A multi-turn completion.
    pub async fn complete_chat(
        &self,
        cfg: llm::Config,
        system: &str,
        messages: Vec<llm::ChatMsg>,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.chat_typed(cfg, system, messages, max_tokens)
            .await
            .map_err(String::from)
    }

    /// [`complete_chat`](AiClient::complete_chat), failing with why, typed.
    async fn chat_typed(
        &self,
        cfg: llm::Config,
        system: &str,
        messages: Vec<llm::ChatMsg>,
        max_tokens: u32,
    ) -> Result<String, CallError> {
        if let Some(tx) = self.server() {
            let req = clew_protocol::Request::Chat {
                system: system.to_string(),
                messages: messages
                    .iter()
                    .map(|m| clew_protocol::AiChatMsg {
                        role: m.role_str().to_string(),
                        content: m.content.clone(),
                    })
                    .collect(),
                max_tokens,
            };
            return match self.rpc(tx, req).await.map_err(CallError::Rpc)? {
                clew_protocol::Event::ChatResult { text } => Ok(text),
                other => Err(CallError::Internal(format!(
                    "unexpected reply to Chat: {}",
                    event_name(&other)
                ))),
            };
        }
        // Client endpoint: call the provider directly (blocking HTTP off-thread).
        // The call polls a flag this future raises when it is dropped — its
        // task aborted (a cancelled explain pass, a project switch) — so an
        // abandoned completion stops instead of running to the end on the
        // meter with nobody left to read it (the server's `Chat` is stopped
        // the same way, by `Awaiting`'s `Cancel`).
        let system = system.to_string();
        let abandoned = RaiseOnDrop::default();
        let stop = abandoned.flag();
        tokio::task::spawn_blocking(move || {
            llm::complete_chat_full(&cfg, &system, &messages, max_tokens, &|| {
                stop.load(std::sync::atomic::Ordering::Relaxed)
            })
            .map(llm::Completion::into_text_with_note)
            .map_err(CallError::Llm)
        })
        .await
        .unwrap_or_else(|_| Err(CallError::Internal("task join failed".into())))
    }

    /// Embed texts — stopped, like a completion, when this future is dropped
    /// (its task aborted: the index build of a project the window left): on
    /// the server by the `Cancel` that follows an abandoned `Embed`
    /// ([`Awaiting`]), and on this machine by the flag the blocking batches
    /// poll, which shuts the connection of the one in flight down.
    pub async fn embed(
        &self,
        cfg: embed::Config,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, String> {
        if let Some(tx) = self.server() {
            return match self
                .rpc(tx, clew_protocol::Request::Embed { texts })
                .await?
            {
                clew_protocol::Event::Embeddings { vecs } => Ok(vecs),
                other => Err(format!("unexpected reply to Embed: {}", event_name(&other))),
            };
        }
        let abandoned = RaiseOnDrop::default();
        let stop = abandoned.flag();
        tokio::task::spawn_blocking(move || {
            embed::embed_all(&cfg, &texts, &|| {
                stop.load(std::sync::atomic::Ordering::Relaxed)
            })
        })
        .await
        .unwrap_or_else(|_| Err("task join failed".into()))
    }
}

/// One request awaiting its correlated reply, and the bookkeeping that must be
/// undone however the wait ends (see [`AiClient::rpc_once`]): dropping this
/// removes the pending entry and — for a request that was sent and that the
/// server can stop (`cancel`, a `Chat` or an `Embed`), when no reply has
/// claimed the entry yet — tells the server to stop it (`Cancel { id }`).
///
/// The `Cancel` is best effort: it goes out without waiting (a destructor
/// cannot), so a server whose request queue is full does not get it, and its
/// own deadline ends the call instead. Its answer to the cancelled request
/// (an `Error` with `ErrorCode::Cancelled`) finds no pending entry and is
/// dropped as the answer to work this client stopped.
struct Awaiting<'a> {
    id: u64,
    pending: &'a std::sync::Mutex<HashMap<u64, PendingReply>>,
    cancel: Option<(&'a server::RequestTx, &'a std::sync::atomic::AtomicU64)>,
    /// The request reached the transport; before that there is nothing on
    /// the server to stop.
    sent: bool,
}

impl Drop for Awaiting<'_> {
    fn drop(&mut self) {
        let outstanding = lock_replies(self.pending).remove(&self.id).is_some();
        if outstanding
            && self.sent
            && let Some((tx, next_id)) = self.cancel
        {
            let _ = send_request(tx, next_id, clew_protocol::Request::Cancel { id: self.id });
        }
    }
}

/// An event's variant name, for "unexpected reply" errors — naming it is
/// enough, and dumping it would put a whole file's content in a status line.
pub(crate) fn event_name(event: &clew_protocol::Event) -> &'static str {
    use clew_protocol::Event;
    match event {
        Event::Ready { .. } => "Ready",
        Event::Tree { .. } => "Tree",
        Event::FileContent { .. } => "FileContent",
        Event::NotebookContent { .. } => "NotebookContent",
        Event::GitInfo { .. } => "GitInfo",
        Event::Stats { .. } => "Stats",
        Event::ProjectCalls { .. } => "ProjectCalls",
        Event::Sources { .. } => "Sources",
        Event::GitResult { .. } => "GitResult",
        Event::StateContent { .. } => "StateContent",
        Event::StateWritten { .. } => "StateWritten",
        Event::StateEdited { .. } => "StateEdited",
        Event::SearchResults { .. } => "SearchResults",
        Event::FilesChanged { .. } => "FilesChanged",
        Event::ProjectSymbols { .. } => "ProjectSymbols",
        Event::ProcessOutput { .. } => "ProcessOutput",
        Event::ProcessStarted { .. } => "ProcessStarted",
        Event::ProcessExited { .. } => "ProcessExited",
        Event::AdapterSpawned { .. } => "AdapterSpawned",
        Event::ChatResult { .. } => "ChatResult",
        Event::ChatDelta { .. } => "ChatDelta",
        Event::ChatStreamDone { .. } => "ChatStreamDone",
        Event::Embeddings { .. } => "Embeddings",
        Event::DirListing { .. } => "DirListing",
        Event::Docs { .. } => "Docs",
        Event::AgentStep { .. } => "AgentStep",
        Event::AgentDelta { .. } => "AgentDelta",
        Event::AgentDone { .. } => "AgentDone",
        Event::Status { .. } => "Status",
        Event::LspResolved { .. } => "LspResolved",
        Event::Error { .. } => "Error",
    }
}

/// Send a fire-and-forget request (one with no reply to wait for — a spawn,
/// stdin bytes, a kill, a cancel, the AI config) under a fresh id from the
/// window's request counter, the one every correlated request is minted from.
/// Fire-and-forget does not mean anonymous: the server answers the ones that
/// fail with an `Error` under that id, and a shared placeholder id made those
/// indistinguishable from each other and from the handshake's. Returns whether
/// the request was queued.
///
/// Never blocks (it runs on the UI thread): a request the bounded queue has
/// no room for is refused — `false`, like a closed channel — never waited on
/// (see `server::REQUEST_QUEUE`). Off the UI thread, use
/// [`send_request_async`], which waits for room instead.
pub(crate) fn send_request(
    tx: &server::RequestTx,
    next_id: &std::sync::atomic::AtomicU64,
    request: clew_protocol::Request,
) -> bool {
    let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tx.try_send(clew_protocol::ClientMessage { id, request })
        .is_ok()
}

/// [`send_request`] for a background task: a full queue is waited out
/// (backpressure), since nothing there is blocked by the wait and a dropped
/// request — a chunk of a proxied process's stdin — would corrupt what it
/// carries. `false` only once the transport is gone.
pub(crate) async fn send_request_async(
    tx: &server::RequestTx,
    next_id: &std::sync::atomic::AtomicU64,
    request: clew_protocol::Request,
) -> bool {
    let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tx.send(clew_protocol::ClientMessage { id, request })
        .await
        .is_ok()
}

/// Where a project's git history is read: this machine (`git` run against the
/// local root on the blocking pool) or the connected server (a remote
/// repository, over the protocol). The one place that choice is made — the git
/// consumers (time travel, diff, "why is this here", the change review) used
/// to carry a hand-copied local arm and remote arm each.
///
/// Both arms run the same dispatch (`clew_core::git::run_op`) and answer the
/// same typed [`clew_protocol::GitResult`] — or an `Err` with the reason git
/// could not answer (remotely the server's `Event::Error`, locally the
/// `GitError` itself). Neither arm ever turns a failure into an empty answer.
#[derive(Clone)]
pub(crate) enum GitSource {
    Local(PathBuf),
    Remote(AiClient),
}

impl GitSource {
    /// Run `op` and take its answer as `T` — which must be the type of the
    /// op's own result (see [`GitAnswer`]); an answer to a different
    /// operation is an error, never a default.
    pub(crate) async fn run<T>(&self, op: clew_protocol::GitOp) -> Result<T, String>
    where
        T: GitAnswer + Send + 'static,
    {
        let asked = op.clone();
        let result = match self {
            GitSource::Remote(ai) => ai.git(op).await?,
            GitSource::Local(root) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || git::run_op(&root, op))
                    .await
                    .map_err(|_| "git task failed".to_string())?
                    .map_err(|e| e.to_string())?
            }
        };
        if !result.answers(&asked) {
            return Err(format!(
                "the git reply answers another operation ({})",
                result.name()
            ));
        }
        let name = result.name();
        T::take(result).ok_or_else(|| format!("the {name} git reply is not the expected shape"))
    }
}

/// A typed answer taken out of the [`clew_protocol::GitResult`] its operation
/// produced — one impl per result type, covering the variants that carry it.
pub(crate) trait GitAnswer: Sized {
    fn take(result: clew_protocol::GitResult) -> Option<Self>;
}

impl GitAnswer for Vec<git::HistCommit> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::FileHistory(v)
            | clew_protocol::GitResult::SymbolHistory(v) => Some(v),
            _ => None,
        }
    }
}

/// `FileAt` (a file's text at a commit) and `CommitMessage`.
impl GitAnswer for Option<String> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::FileAt(v) | clew_protocol::GitResult::CommitMessage(v) => {
                Some(v)
            }
            _ => None,
        }
    }
}

impl GitAnswer for HashSet<usize> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::AddedLines(v) => Some(v),
            _ => None,
        }
    }
}

/// `CommitFileDiff` and `RangePatch` (patch text).
impl GitAnswer for String {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::CommitFileDiff(v)
            | clew_protocol::GitResult::RangePatch(v) => Some(v),
            _ => None,
        }
    }
}

impl GitAnswer for Option<Vec<git::DiffLine>> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::DiffLines(v) => Some(v),
            _ => None,
        }
    }
}

/// `ReviewBase`: `(base, label)`.
impl GitAnswer for Vec<clew_protocol::FileChurn> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::Churn(v) => Some(v),
            _ => None,
        }
    }
}

impl GitAnswer for Option<(String, String)> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::ReviewBase(v) => Some(v),
            _ => None,
        }
    }
}

impl GitAnswer for Vec<String> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::CommitSubjects(v) => Some(v),
            _ => None,
        }
    }
}

/// `ChangedFiles`: `(rel, status letter)`.
impl GitAnswer for Vec<(String, char)> {
    fn take(result: clew_protocol::GitResult) -> Option<Self> {
        match result {
            clew_protocol::GitResult::ChangedFiles(v) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AiClient, AiEndpoint, GitSource, PendingReply};
    use clew_protocol::{ClientMessage, ErrorCode, Event, GitOp, GitResult, Request};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    type Pending = Arc<Mutex<HashMap<u64, PendingReply>>>;

    /// A client wired to a fake server: `answer` decides each request's reply
    /// (by the order it arrived in), and the returned task reports the
    /// requests the server saw.
    fn client(
        answer: impl Fn(usize, &Request) -> Event + Send + 'static,
    ) -> (AiClient, tokio::task::JoinHandle<Vec<Request>>) {
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<ClientMessage>(crate::server::REQUEST_QUEUE);
        let pending: Pending = Arc::default();
        let ai = AiClient {
            endpoint: AiEndpoint::Server,
            server_tx: Some(tx),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: pending.clone(),
        };
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(msg) = rx.recv().await {
                let reply = answer(seen.len(), &msg.request);
                seen.push(msg.request);
                if let Some(otx) = pending.lock().unwrap().remove(&msg.id) {
                    let _ = otx.send(reply);
                }
            }
            seen
        });
        (ai, server)
    }

    /// W2-S S6: a background request meets a full request queue by WAITING
    /// for room (backpressure), not by failing — and not by queueing without
    /// bound: it is sent once the writer takes what was ahead of it.
    #[tokio::test]
    async fn a_full_request_queue_is_waited_out_by_a_background_request() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientMessage>(1);
        let pending: Pending = Arc::default();
        let ai = AiClient {
            endpoint: AiEndpoint::Server,
            server_tx: Some(tx.clone()),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: pending.clone(),
        };
        // The queue is full: one request the "server" has not read yet.
        tx.try_send(ClientMessage {
            id: 999,
            request: Request::Stats,
        })
        .unwrap();
        let call = tokio::spawn(async move { ai.request(Request::Stats).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!call.is_finished(), "the request did not wait for room");
        // The server reads again: the waiting request goes out and is answered.
        assert_eq!(rx.recv().await.unwrap().id, 999);
        let queued = rx.recv().await.expect("the waiting request was sent");
        let otx = pending.lock().unwrap().remove(&queued.id).unwrap();
        let _ = otx.send(Event::Stats {
            root: "/p".into(),
            report: clew_protocol::StatsReport::default(),
        });
        let got = call.await.unwrap();
        assert!(matches!(got, Ok(Event::Stats { .. })), "{got:?}");
    }

    /// The scan-window refusal is retried because of its CODE — whatever its
    /// wording says — and the retry's answer is the caller's.
    #[tokio::test]
    async fn a_not_ready_refusal_is_retried_by_its_code() {
        let (ai, server) = client(|n, _| match n {
            0 => Event::error(ErrorCode::NotReady, "reworded: still scanning"),
            _ => Event::Stats {
                root: "/p".into(),
                report: clew_protocol::StatsReport::default(),
            },
        });
        let got = ai.request(Request::Stats).await;
        assert!(matches!(got, Ok(Event::Stats { .. })), "{got:?}");
        drop(ai);
        assert_eq!(server.await.unwrap().len(), 2, "one refusal, one retry");
    }

    /// Any other code is final: no retry, and the server's message is the
    /// caller's error.
    #[tokio::test]
    async fn other_errors_are_final() {
        let (ai, server) = client(|_, _| Event::error(ErrorCode::Refused, "refused: bad path"));
        let got = ai.request(Request::Stats).await;
        assert_eq!(got.unwrap_err(), "refused: bad path");
        drop(ai);
        assert_eq!(server.await.unwrap().len(), 1, "a refusal is not retried");
    }

    /// Even a NotReady is never retried for a request that is not idempotent:
    /// re-sending it could apply it twice.
    #[tokio::test]
    async fn a_non_idempotent_request_is_never_resent() {
        let (ai, server) = client(|_, _| Event::error(ErrorCode::NotReady, "not ready"));
        let got = ai
            .request(Request::WriteState {
                root: "/p".into(),
                rel: "history.json".into(),
                text: None,
            })
            .await;
        assert!(got.is_err());
        drop(ai);
        assert_eq!(server.await.unwrap().len(), 1);
    }

    /// CP#4: dropping a `Chat` that waits for its reply — its task aborted
    /// (a cancelled explain pass, a project switch) — tells the server to
    /// stop it, by the Chat's own id, and forgets the pending entry: the
    /// provider call used to run (and bill) for up to ten minutes with nobody
    /// left to read it. A request the server cannot stop is only forgotten,
    /// and an answered Chat sends no Cancel.
    #[tokio::test]
    async fn an_abandoned_chat_is_cancelled_on_the_server() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientMessage>(8);
        let pending: Pending = Arc::default();
        let ai = AiClient {
            endpoint: AiEndpoint::Server,
            server_tx: Some(tx),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: pending.clone(),
        };
        let cfg = || {
            crate::llm::Config::from_parts(
                crate::llm::Provider::OpenAI,
                "sk-test".into(),
                "test-model".into(),
                "http://127.0.0.1:9/v1".into(),
            )
        };
        let chat = |ai: AiClient| {
            let cfg = cfg();
            tokio::spawn(async move {
                ai.complete_chat(cfg, "system", vec![crate::llm::ChatMsg::user("q")], 16)
                    .await
            })
        };

        let call = chat(ai.clone());
        let sent = rx.recv().await.unwrap();
        assert!(matches!(sent.request, Request::Chat { .. }));
        assert!(pending.lock().unwrap().contains_key(&sent.id));
        call.abort();
        let _ = call.await;
        let cancel = rx
            .recv()
            .await
            .expect("a Cancel followed the abandoned Chat");
        assert!(
            matches!(cancel.request, Request::Cancel { id } if id == sent.id),
            "{:?}",
            cancel.request
        );
        assert!(pending.lock().unwrap().is_empty(), "the entry leaked");

        // Not cancellable: only forgotten.
        let stats = {
            let ai = ai.clone();
            tokio::spawn(async move { ai.request(Request::Stats).await })
        };
        let _ = rx.recv().await.unwrap();
        stats.abort();
        let _ = stats.await;
        assert!(pending.lock().unwrap().is_empty(), "the entry leaked");
        assert!(
            rx.try_recv().is_err(),
            "a Cancel for a request that has none"
        );

        // Answered: nothing to stop.
        let call = chat(ai.clone());
        let sent = rx.recv().await.unwrap();
        let otx = pending.lock().unwrap().remove(&sent.id).unwrap();
        let _ = otx.send(Event::ChatResult {
            text: "answer".into(),
        });
        assert_eq!(call.await.unwrap().unwrap(), "answer");
        assert!(rx.try_recv().is_err(), "an answered Chat was cancelled");
    }

    /// An `Embed` is stopped as a `Chat` is: dropping it while it waits — its
    /// task aborted with the index build of a project the window left —
    /// tells the server, by its own id, and the server stops embedding. It
    /// used to be only forgotten, and the server embedded, and billed, every
    /// batch it had left for nobody.
    #[tokio::test]
    async fn an_abandoned_embed_is_cancelled_on_the_server() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ClientMessage>(8);
        let pending: Pending = Arc::default();
        let ai = AiClient {
            endpoint: AiEndpoint::Server,
            server_tx: Some(tx),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: pending.clone(),
        };
        let cfg = crate::embed::Config::from_parts("sk".into(), "m".into(), String::new());
        let call = tokio::spawn(async move { ai.embed(cfg, vec!["x".into()]).await });
        let sent = rx.recv().await.unwrap();
        assert!(matches!(sent.request, Request::Embed { .. }));
        call.abort();
        let _ = call.await;
        let cancel = rx
            .recv()
            .await
            .expect("a Cancel followed the abandoned Embed");
        assert!(
            matches!(cancel.request, Request::Cancel { id } if id == sent.id),
            "{:?}",
            cancel.request
        );
        assert!(pending.lock().unwrap().is_empty(), "the entry leaked");
    }

    /// The same on this machine: dropping an `embed` whose batch is out —
    /// its task aborted — gives the batch up, and the endpoint sees its
    /// connection close. The batch used to run on, holding a blocking
    /// thread and the connection, until the endpoint answered or the
    /// request's own limits ran out.
    #[tokio::test]
    async fn an_abandoned_local_embed_lets_its_endpoint_go() {
        use std::time::{Duration, Instant};
        let (base, arrived, closed) = clew_core::testutil::silent_http_endpoint();
        let ai = AiClient {
            endpoint: AiEndpoint::Client,
            server_tx: None,
            next_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::default(),
        };
        let cfg = crate::embed::Config::from_parts("sk".into(), "m".into(), base);
        let call = tokio::spawn(async move { ai.embed(cfg, vec!["x".into()]).await });
        let out_there =
            tokio::task::spawn_blocking(move || arrived.recv_timeout(Duration::from_secs(10)));
        assert!(
            out_there.await.unwrap().is_ok(),
            "the batch never reached the endpoint"
        );
        let abandoned_at = Instant::now();
        call.abort();
        let _ = call.await;
        let (gone, at) =
            tokio::task::spawn_blocking(move || closed.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap()
                .expect("the abandoned batch kept its connection");
        assert!(gone);
        assert!(
            at.saturating_duration_since(abandoned_at) < Duration::from_secs(2),
            "the connection closed {:?} after the abort",
            at.saturating_duration_since(abandoned_at)
        );
    }

    /// The seam between a future and the blocking provider call it spawned:
    /// aborting the future's task raises the flag the call polls.
    #[tokio::test]
    async fn an_aborted_task_raises_its_cancel_flag() {
        let guard = crate::app::model::RaiseOnDrop::default();
        let flag = guard.flag();
        let task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
        task.abort();
        let _ = task.await;
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    }

    /// A git answer is typed end to end, and one that answers a different
    /// operation — or is not a git answer at all — is an error, never a
    /// default an empty history could be mistaken for.
    #[tokio::test]
    async fn git_answers_are_checked_against_the_operation() {
        let commit = clew_protocol::HistCommit {
            sha: "abc".into(),
            author: "a".into(),
            time: 1,
            subject: "initial".into(),
            path: "a.rs".into(),
        };
        let history = GitOp::FileHistory {
            rel: "a.rs".into(),
            limit: 5,
        };

        let reply = commit.clone();
        let (ai, _server) = client(move |_, _| Event::GitResult {
            root: "/p".into(),
            result: GitResult::FileHistory(vec![reply.clone()]),
        });
        let got: Vec<clew_protocol::HistCommit> =
            GitSource::Remote(ai).run(history.clone()).await.unwrap();
        assert_eq!(got, vec![commit]);

        let (ai, _server) = client(|_, _| Event::GitResult {
            root: "/p".into(),
            result: GitResult::FileAt(None),
        });
        let err = GitSource::Remote(ai)
            .run::<Vec<clew_protocol::HistCommit>>(history.clone())
            .await
            .unwrap_err();
        assert!(err.contains("another operation"), "{err}");

        let (ai, _server) = client(|_, _| Event::ChatResult { text: "?".into() });
        let err = GitSource::Remote(ai)
            .run::<Vec<clew_protocol::HistCommit>>(history)
            .await
            .unwrap_err();
        assert!(err.contains("unexpected reply"), "{err}");
    }

    /// The local arm answers through the same typed dispatch the server runs
    /// — and, like the remote arm, reports a git that could not answer as an
    /// error, never as the op's empty value.
    #[tokio::test]
    async fn the_local_arm_answers_the_same_types() {
        let dir = crate::app::tests::test_dir("gitsource-local");
        std::fs::create_dir_all(&dir).unwrap();
        let source = GitSource::Local(dir.clone());
        // Not a repository: "no history" would be a false answer.
        let err = source
            .run::<Vec<clew_protocol::HistCommit>>(GitOp::FileHistory {
                rel: "a.rs".into(),
                limit: 5,
            })
            .await
            .unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");
        assert!(
            source
                .run::<Option<(String, String)>>(GitOp::ReviewBase)
                .await
                .is_err()
        );
        // The diff view's `None` ("nothing to diff") is a real answer.
        let diff: Option<Vec<clew_protocol::DiffLine>> = source
            .run(GitOp::DiffLines { rel: "a.rs".into() })
            .await
            .unwrap();
        assert_eq!(diff, None);
    }
}
