//! MCP's Streamable HTTP transport, client side: every message is its own
//! POST to the server's one endpoint, answered with a JSON body or with a
//! server-sent-event stream carrying notifications and then the response.
//!
//! Both eras ride on it (see the parent module). A modern request mirrors
//! its method, its target and the arguments the tool marks with
//! `x-mcp-header` into headers (`Mcp-Method`, `Mcp-Name`, `Mcp-Param-*`)
//! next to `MCP-Protocol-Version`. A legacy connection carries the session
//! the server minted at `initialize` (`Mcp-Session-Id`), re-initializes once
//! when the server says the session is gone (404), answers the requests a
//! legacy server may send in a response stream, and deletes its session when
//! closed.
//!
//! Each POST runs on a thread of its own whose connection is tethered to the
//! call ([`crate::net::Line`]): a call that is given up on — its time box, a
//! Stop — shuts its connection down, which a modern server takes for the
//! request's cancellation.

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    Era, LEGACY_VERSIONS, McpError, Probe, ServerInfo, ToolInfo, header_value, initialize_params,
    modern_info, param_headers, read_initialize, read_probe, with_meta,
};
use crate::linerpc::RpcError;

/// The longest any read on an MCP connection may stay silent — the wait for
/// a slow tool's answer included. Each call is bounded by its own timeout
/// as well.
const READ_IDLE: Duration = Duration::from_secs(600);

/// Largest JSON response body read.
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// Largest error body read: enough to find a JSON-RPC error in it.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// Largest single server-sent event.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// How long the check for an old HTTP+SSE server may take.
const OLD_SSE_PROBE: Duration = Duration::from_secs(10);

/// How long a legacy session's DELETE may take when the client closes.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request waits for the sign-in's token to be read or renewed:
/// a renewal is one bounded request, and may first wait for another
/// process's renewal to finish (the store is locked across both).
const SIGN_IN_WAIT: Duration = Duration::from_secs(75);

/// The connection state the requests share.
#[derive(Debug, Default)]
struct State {
    /// The era agreed on at open.
    era: Option<Era>,
    /// A legacy server's session, from its `initialize` answer.
    session: Option<String>,
}

/// A Streamable HTTP endpoint.
pub(crate) struct HttpLink {
    agent: ureq::Agent,
    uri: ureq::http::Uri,
    url: String,
    /// The user's own headers (credentials), sent with every request.
    headers: Vec<(String, String)>,
    state: Arc<Mutex<State>>,
    stale: Arc<AtomicBool>,
    ids: AtomicI64,
    /// The access token of the user's sign-in to this server, when it has
    /// one and no Authorization header was configured ([`super::oauth`]).
    token: Mutex<Option<String>>,
    /// Held while the token is renewed: requests refused at once renew it
    /// once, not each with the same refresh token (which a server rotating
    /// them takes for a stolen one).
    renewing: tokio::sync::Mutex<()>,
}

/// What a POST came back with.
enum Posted {
    /// 202: a notification (or a response of ours) was taken.
    Accepted,
    /// The JSON-RPC response to the request.
    Response(Value),
}

/// A failed POST, with the HTTP status when there was one.
struct Failed {
    status: Option<u16>,
    error: McpError,
}

impl From<McpError> for Failed {
    fn from(error: McpError) -> Self {
        Failed {
            status: None,
            error,
        }
    }
}

/// Everything one POST needs, owned, for its thread.
#[derive(Clone)]
struct Post {
    agent: ureq::Agent,
    uri: ureq::http::Uri,
    headers: Vec<(String, String)>,
    stale: Arc<AtomicBool>,
}

impl HttpLink {
    pub(crate) fn new(
        url: &str,
        headers: &[(String, String)],
        stale: Arc<AtomicBool>,
    ) -> Result<HttpLink, McpError> {
        let url = url.trim();
        let parsed = url::Url::parse(url)
            .map_err(|e| McpError::Config(format!("{url} is not a URL: {e}")))?;
        match parsed.scheme() {
            "https" => {}
            "http" if crate::net::is_loopback_http(url) => {}
            "http" => {
                return Err(McpError::Config(format!(
                    "{url}: plain http to another machine would send everything in the clear — use https"
                )));
            }
            other => {
                return Err(McpError::Config(format!(
                    "{url}: an MCP server is reached over http(s), not {other}"
                )));
            }
        }
        let (agent, _via) = crate::llm::agent_for(url, crate::llm::Limits::streaming(READ_IDLE))
            .map_err(|e| McpError::Config(e.to_string()))?;
        let uri = crate::net::request_uri(url).map_err(McpError::Config)?;
        Ok(HttpLink {
            agent,
            uri,
            url: url.to_string(),
            headers: headers.to_vec(),
            state: Arc::new(Mutex::new(State::default())),
            stale,
            ids: AtomicI64::new(0),
            token: Mutex::new(None),
            renewing: tokio::sync::Mutex::new(()),
        })
    }

    /// Whether this connection authenticates with a sign-in (no
    /// Authorization header of the user's own).
    fn uses_sign_in(&self) -> bool {
        !self
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    }

    /// Take up the stored sign-in's access token (refreshed if it expired).
    async fn load_token(&self) {
        if !self.uses_sign_in() {
            return;
        }
        let url = self.url.clone();
        let token = tokio::time::timeout(
            SIGN_IN_WAIT,
            tokio::task::spawn_blocking(move || super::oauth::bearer(&url)),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) = token;
    }

    fn token(&self) -> Option<String> {
        self.token
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// After a 401 to a request sent with the token `refused`: renew the
    /// sign-in's access token, unless another request already did. Whether
    /// there is a new one to try.
    async fn renew_token(&self, refused: Option<String>) -> bool {
        if !self.uses_sign_in() || !super::oauth::signed_in(&self.url) {
            return false;
        }
        let _one = self.renewing.lock().await;
        if let Some(current) = self.token()
            && Some(&current) != refused.as_ref()
        {
            return true;
        }
        let url = self.url.clone();
        let renewed = tokio::time::timeout(
            SIGN_IN_WAIT,
            tokio::task::spawn_blocking(move || super::oauth::refresh(&url)),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(Result::ok);
        let ok = renewed.is_some();
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) = renewed;
        ok
    }

    /// An Unauthorized error as this connection explains it: a sign-in is
    /// the way out only for a server reached without a header of the user's.
    fn unauthorized(&self, error: McpError) -> McpError {
        match error {
            McpError::Unauthorized {
                status,
                detail,
                sign_in,
            } => McpError::Unauthorized {
                status,
                detail,
                sign_in: sign_in && self.uses_sign_in(),
            },
            other => other,
        }
    }

    fn next_id(&self) -> i64 {
        self.ids.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The user's headers and the sign-in's token, as every request to the
    /// server carries them (a signed-in server refuses a DELETE or a probe
    /// without its token, and the session it holds is never ended).
    fn credential_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if let Some(token) = self.token() {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        headers
    }

    /// The headers every POST of this connection carries now.
    fn base_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if let Some(token) = self
            .token
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        let state = self.state();
        if let Some(era) = &state.era {
            headers.push(("MCP-Protocol-Version".into(), era.version().to_string()));
        }
        if let Some(session) = &state.session {
            headers.push(("Mcp-Session-Id".into(), session.clone()));
        }
        headers
    }

    fn post_ctx(&self, extra: Vec<(String, String)>) -> Post {
        let mut headers = self.base_headers();
        headers.extend(extra);
        Post {
            agent: self.agent.clone(),
            uri: self.uri.clone(),
            headers,
            stale: self.stale.clone(),
        }
    }

    /// Probe the era and, for a legacy server, run the handshake — signed
    /// in, when the user signed in to the server (renewing an access token
    /// it refuses, once).
    pub(crate) async fn open(&self) -> Result<ServerInfo, McpError> {
        self.load_token().await;
        let sent = self.token();
        match self.open_once().await {
            Err(McpError::Unauthorized { .. }) if self.renew_token(sent).await => {
                self.open_once().await.map_err(|e| self.unauthorized(e))
            }
            other => other.map_err(|e| self.unauthorized(e)),
        }
    }

    async fn open_once(&self) -> Result<ServerInfo, McpError> {
        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": "server/discover",
            "params": with_meta(json!({}), super::MODERN_VERSION),
        });
        let probe_headers = vec![
            ("MCP-Protocol-Version".into(), super::MODERN_VERSION.into()),
            ("Mcp-Method".into(), "server/discover".into()),
        ];
        let ctx = self.post_ctx(probe_headers);
        let outcome = run_post(ctx, body, id, super::PROBE_TIMEOUT).await;
        let probe = match outcome {
            Ok(Posted::Response(msg)) => match rpc_error(&msg) {
                Some(err) => Err(Some(err)),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            },
            Ok(Posted::Accepted) => Err(None),
            Err(Failed {
                error: e @ (McpError::Unauthorized { .. } | McpError::Config(_)),
                ..
            }) => return Err(e),
            Err(Failed {
                error:
                    McpError::Rpc {
                        code,
                        message,
                        data,
                    },
                ..
            }) => Err(Some(RpcError {
                code,
                message,
                data,
            })),
            // An HTTP status with no JSON-RPC error: a legacy server (or an
            // old HTTP+SSE one, which the handshake below finds out).
            Err(Failed {
                status: Some(_), ..
            }) => Err(None),
            Err(Failed { error, .. }) => return Err(error),
        };
        match read_probe(probe)? {
            Probe::Modern(version, discover) => {
                self.state().era = Some(Era::Modern(version.clone()));
                Ok(modern_info(version, discover.as_ref()))
            }
            Probe::Legacy(version) => self.initialize(&version).await,
        }
    }

    /// The legacy handshake: `initialize` (which may mint a session), then
    /// `notifications/initialized`.
    async fn initialize(&self, version: &str) -> Result<ServerInfo, McpError> {
        {
            let mut state = self.state();
            state.era = None;
            state.session = None;
        }
        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": initialize_params(version),
        });
        let ctx = self.post_ctx(Vec::new());
        let outcome = run_blocking(super::PROBE_TIMEOUT, move || {
            post_capturing_session(&ctx, &body, id)
        })
        .await;
        let (posted, session) = match outcome {
            Ok(Ok(posted)) => posted,
            Ok(Err(failed)) | Err(failed) => {
                // An endpoint that refuses the handshake as well may be one
                // of the old HTTP+SSE transport: name it as one.
                if matches!(failed.status, Some(400 | 404 | 405)) && self.is_old_sse().await {
                    return Err(McpError::OldSse);
                }
                return Err(failed.error);
            }
        };
        let result = match posted {
            Posted::Response(msg) => {
                if let Some(err) = rpc_error(&msg) {
                    return Err(McpError::Rpc {
                        code: err.code,
                        message: err.message,
                        data: err.data,
                    });
                }
                msg.get("result").cloned().unwrap_or(Value::Null)
            }
            Posted::Accepted => {
                return Err(McpError::Protocol("initialize was not answered".into()));
            }
        };
        let info = read_initialize(&result)?;
        {
            let mut state = self.state();
            state.era = Some(info.era.clone());
            state.session = session;
        }
        let ctx = self.post_ctx(Vec::new());
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        // The server owes nothing but a 202; a failure here shows on the
        // first real request.
        let _ = run_blocking(super::PROBE_TIMEOUT, move || post(&ctx, &note, None)).await;
        Ok(info)
    }

    /// One request. A legacy session the server forgot (404) is opened
    /// again, once, and the request sent again in the new one; an access
    /// token the server refuses (401: it expired) is renewed, once, and the
    /// request sent again with the new one.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        tool: Option<&ToolInfo>,
    ) -> Result<Value, McpError> {
        let modern = self.state().era.as_ref().is_some_and(Era::is_modern);
        let (mut reopened, mut renewed) = (false, false);
        loop {
            let id = self.next_id();
            let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            let mut extra = Vec::new();
            if modern {
                extra.push(("Mcp-Method".to_string(), method.to_string()));
                let target = params
                    .get("name")
                    .or_else(|| params.get("uri"))
                    .and_then(Value::as_str)
                    .filter(|_| matches!(method, "tools/call" | "resources/read" | "prompts/get"));
                if let Some(target) = target {
                    extra.push(("Mcp-Name".to_string(), header_value(target)));
                }
                if let (Some(tool), Some(args)) = (tool, params.get("arguments")) {
                    extra.extend(param_headers(tool, args));
                }
            }
            let had_session = self.state().session.is_some();
            let sent = self.token();
            let ctx = self.post_ctx(extra);
            match run_post(ctx, body, id, timeout).await {
                Ok(Posted::Response(msg)) => {
                    return match rpc_error(&msg) {
                        Some(err) => Err(McpError::Rpc {
                            code: err.code,
                            message: err.message,
                            data: err.data,
                        }),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                }
                Ok(Posted::Accepted) => {
                    return Err(McpError::Protocol(format!("`{method}` was not answered")));
                }
                Err(Failed {
                    status: Some(404), ..
                }) if had_session && !reopened => {
                    reopened = true;
                    let version = self
                        .state()
                        .era
                        .as_ref()
                        .map(|e| e.version().to_string())
                        .unwrap_or_else(|| LEGACY_VERSIONS[0].to_string());
                    self.initialize(&version).await?;
                }
                Err(Failed {
                    error: McpError::Unauthorized { .. },
                    ..
                }) if !renewed && self.renew_token(sent).await => renewed = true,
                Err(Failed {
                    status: Some(404), ..
                }) if had_session => {
                    return Err(McpError::Protocol(
                        "the server forgot its session again".into(),
                    ));
                }
                Err(failed) => return Err(self.unauthorized(failed.error)),
            }
        }
    }

    /// Whether the endpoint answers a GET with the `endpoint` event the old
    /// HTTP+SSE transport (2024-11-05) opens its stream with.
    async fn is_old_sse(&self) -> bool {
        let agent = self.agent.clone();
        let uri = self.uri.clone();
        let headers = self.credential_headers();
        let first = run_blocking(OLD_SSE_PROBE, move || {
            let response = crate::net::guarded(|| {
                let mut request = agent.get(uri).header("Accept", "text/event-stream");
                for (name, value) in &headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                request.call()
            })
            .ok()?;
            let streams = response.status().as_u16() == 200
                && response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|t| t.to_ascii_lowercase().starts_with("text/event-stream"));
            if !streams {
                return None;
            }
            SseReader::new(response.into_body().into_reader())
                .next_event()
                .ok()
                .flatten()
        })
        .await;
        matches!(first, Ok(Some(event)) if event.event == "endpoint")
    }

    /// End a legacy session (DELETE); a modern connection holds none.
    pub(crate) async fn close(&self) {
        let Some(session) = self.state().session.take() else {
            return;
        };
        let agent = self.agent.clone();
        let uri = self.uri.clone();
        let headers = self.credential_headers();
        let _ = run_blocking(CLOSE_TIMEOUT, move || {
            crate::net::guarded(|| {
                let mut request = agent.delete(uri);
                for (name, value) in &headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                request.header("Mcp-Session-Id", session.as_str()).call()
            })
            .map(|_| ())
            .map_err(|e| Failed::from(McpError::Transport(crate::net::describe(&e))))
        })
        .await;
    }

    /// The endpoint, for messages.
    #[allow(dead_code)]
    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

/// The JSON-RPC error of a response, if it is one.
fn rpc_error(msg: &Value) -> Option<RpcError> {
    let err = msg.get("error")?;
    Some(RpcError {
        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string(),
        data: err.get("data").cloned(),
    })
}

/// [`post`] on a thread of its own, for at most `timeout`.
async fn run_post(ctx: Post, body: Value, id: i64, timeout: Duration) -> Result<Posted, Failed> {
    run_blocking(timeout, move || post(&ctx, &body, Some(id))).await?
}

/// Run `work` on a thread whose connections are tethered to this call: when
/// the call is dropped or its time runs out, they are shut down under
/// whatever the thread is blocked in.
async fn run_blocking<T: Send + 'static>(
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Failed> {
    struct CloseOnDrop(Arc<crate::net::Line>);
    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            self.0.close();
        }
    }
    let line = Arc::new(crate::net::Line::default());
    let worker_line = line.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("clew-mcp".into())
        .spawn(move || {
            let _tether = crate::net::Line::tether(worker_line);
            let _ = tx.send(work());
        })
        .map_err(|e| Failed::from(McpError::Transport(format!("no request thread: {e}"))))?;
    let _close = CloseOnDrop(line);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(McpError::Transport("the request thread stopped".into()).into()),
        Err(_) => Err(McpError::TimedOut.into()),
    }
}

/// The handshake's POST, which also reads the session the server minted.
fn post_capturing_session(
    ctx: &Post,
    body: &Value,
    id: i64,
) -> Result<(Posted, Option<String>), Failed> {
    let response = send(ctx, body)?;
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        .map(str::to_owned);
    let posted = read_response(ctx, response, Some(id))?;
    Ok((posted, session))
}

/// POST one message and read what comes back: for a request (`id`), its
/// response, answering what a legacy server asks in between.
fn post(ctx: &Post, body: &Value, id: Option<i64>) -> Result<Posted, Failed> {
    let response = send(ctx, body)?;
    read_response(ctx, response, id)
}

fn send(ctx: &Post, body: &Value) -> Result<ureq::http::Response<ureq::Body>, Failed> {
    let text = body.to_string();
    crate::net::guarded(|| {
        let mut request = ctx
            .agent
            .post(ctx.uri.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in &ctx.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request.send(text.as_str())
    })
    .map_err(|e| Failed::from(McpError::Transport(crate::net::describe(&e))))
}

fn read_response(
    ctx: &Post,
    response: ureq::http::Response<ureq::Body>,
    id: Option<i64>,
) -> Result<Posted, Failed> {
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    if status == 202 {
        return Ok(Posted::Accepted);
    }
    if (300..400).contains(&status) {
        return Err(Failed {
            status: Some(status),
            error: McpError::Config(format!(
                "the server redirected to {} — configure that URL instead",
                header("location")
            )),
        });
    }
    if status == 401 || status == 403 {
        return Err(Failed {
            status: Some(status),
            error: McpError::Unauthorized {
                status,
                sign_in: super::oauth::parse_challenge(&header("www-authenticate")).is_some(),
                detail: header("www-authenticate"),
            },
        });
    }
    let content_type = header("content-type").to_ascii_lowercase();
    if !(200..300).contains(&status) {
        let mut body = String::new();
        let _ = response
            .into_body()
            .into_reader()
            .take(MAX_ERROR_BODY_BYTES)
            .read_to_string(&mut body);
        let error = match serde_json::from_str::<Value>(&body)
            .ok()
            .as_ref()
            .and_then(rpc_error)
        {
            Some(err) => McpError::Rpc {
                code: err.code,
                message: err.message,
                data: err.data,
            },
            None => McpError::Http {
                status,
                body: body.trim().chars().take(300).collect(),
            },
        };
        return Err(Failed {
            status: Some(status),
            error,
        });
    }
    let Some(id) = id else {
        return Ok(Posted::Accepted);
    };
    let reader = response.into_body().into_reader();
    if content_type.starts_with("text/event-stream") {
        return read_event_stream(ctx, reader, id);
    }
    let mut bytes = Vec::new();
    reader
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Failed::from(McpError::Transport(crate::net::describe_io(&e))))?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(McpError::Protocol("a response over 32 MB".into()).into());
    }
    let msg: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Failed::from(McpError::Protocol(format!("a body that is not JSON: {e}"))))?;
    let mut found = None;
    for msg in batch(msg) {
        if let Some(response) = handle_message(ctx, msg, id)? {
            found = Some(response);
        }
    }
    found
        .map(Posted::Response)
        .ok_or_else(|| McpError::Protocol("the body held no answer to the request".into()).into())
}

/// A message, or the members of a batch.
fn batch(msg: Value) -> Vec<Value> {
    match msg {
        Value::Array(items) => items,
        msg => vec![msg],
    }
}

/// Read a response stream until the response to `id` arrives in it.
fn read_event_stream(ctx: &Post, reader: impl Read, id: i64) -> Result<Posted, Failed> {
    let mut events = SseReader::new(reader);
    loop {
        match events.next_event() {
            Ok(Some(event)) => {
                let Ok(msg) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                for msg in batch(msg) {
                    if let Some(response) = handle_message(ctx, msg, id)? {
                        return Ok(Posted::Response(response));
                    }
                }
            }
            Ok(None) => {
                return Err(McpError::Transport(
                    "the server's response stream ended without an answer".into(),
                )
                .into());
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// One message from the server: `Some` when it is the response to `id`.
/// Notifications are folded in; a legacy server's own requests are answered
/// with a POST of their own.
fn handle_message(ctx: &Post, msg: Value, id: i64) -> Result<Option<Value>, Failed> {
    let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
    let msg_id = msg.get("id").filter(|v| !v.is_null()).cloned();
    match (msg_id, method) {
        (Some(msg_id), None) => Ok((msg_id.as_i64() == Some(id)).then_some(msg)),
        (Some(msg_id), Some(method)) => {
            let answer = match method.as_str() {
                "ping" => Ok(json!({})),
                other => Err(RpcError::method_not_found(other)),
            };
            let reply = crate::linerpc::response(&msg_id, answer);
            // Best effort: a server that cannot take our answer will say so
            // on the request it is holding.
            let _ = post(ctx, &reply, None);
            Ok(None)
        }
        (None, Some(method)) => {
            if method == "notifications/tools/list_changed" {
                ctx.stale.store(true, Ordering::Relaxed);
            }
            Ok(None)
        }
        (None, None) => Ok(None),
    }
}

/// One server-sent event: its type (`message` unless it named one) and its
/// `data` lines, joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event: String,
    pub(crate) data: String,
}

/// A reader of server-sent events.
pub(crate) struct SseReader<R> {
    lines: BufReader<R>,
}

impl<R: Read> SseReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        SseReader {
            lines: BufReader::new(reader),
        }
    }

    /// The next event; `None` at the end of the stream. Comments, `id:` and
    /// `retry:` lines carry nothing this client uses.
    pub(crate) fn next_event(&mut self) -> Result<Option<SseEvent>, McpError> {
        let mut data = String::new();
        let mut event = String::new();
        let mut has_data = false;
        let done = |event: String, data: String| SseEvent {
            event: if event.is_empty() {
                "message".into()
            } else {
                event
            },
            data,
        };
        loop {
            let mut line = Vec::new();
            let n = (&mut self.lines)
                .take(MAX_EVENT_BYTES as u64 + 2)
                .read_until(b'\n', &mut line)
                .map_err(|e| McpError::Transport(crate::net::describe_io(&e)))?;
            if n == 0 {
                return Ok(has_data.then(|| done(event, data)));
            }
            if line.len() > MAX_EVENT_BYTES || data.len() > MAX_EVENT_BYTES {
                return Err(McpError::Protocol("a server-sent event over 16 MB".into()));
            }
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if has_data {
                    return Ok(Some(done(event, data)));
                }
                event.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "data" => {
                    if has_data {
                        data.push('\n');
                    }
                    data.push_str(value);
                    has_data = true;
                }
                "event" => event = value.to_string(),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_join_their_data_lines_and_skip_comments() {
        let stream = ": keep-alive\n\nevent: message\ndata: {\"a\":\ndata: 1}\n\nid: 7\ndata:{\"b\":2}\r\n\r\ndata: tail";
        let mut sse = SseReader::new(stream.as_bytes());
        let first = sse.next_event().unwrap().unwrap();
        assert_eq!(
            (first.event.as_str(), first.data.as_str()),
            ("message", "{\"a\":\n1}")
        );
        assert_eq!(sse.next_event().unwrap().unwrap().data, "{\"b\":2}");
        assert_eq!(sse.next_event().unwrap().unwrap().data, "tail");
        assert_eq!(sse.next_event().unwrap(), None);
        let mut old = SseReader::new("event: endpoint\ndata: /messages?s=1\n\n".as_bytes());
        assert_eq!(old.next_event().unwrap().unwrap().event, "endpoint");
    }

    #[test]
    fn plain_http_is_only_for_this_machine() {
        let stale = Arc::new(AtomicBool::new(false));
        assert!(HttpLink::new("http://127.0.0.1:9/mcp", &[], stale.clone()).is_ok());
        assert!(HttpLink::new("https://example.com/mcp", &[], stale.clone()).is_ok());
        assert!(matches!(
            HttpLink::new("http://example.com/mcp", &[], stale.clone()),
            Err(McpError::Config(_))
        ));
        assert!(matches!(
            HttpLink::new("ftp://example.com/mcp", &[], stale),
            Err(McpError::Config(_))
        ));
    }
}
