//! `clew-server acp`: clew's Ask agent as an ACP agent (protocol version 1),
//! for editors that host agents over the Agent Client Protocol — Zed, the
//! JetBrains IDEs, any ACP client.
//!
//! A session is a project: `session/new` names its folder (`cwd`), which is
//! scanned once and rescanned when a turn finds the list a few seconds old.
//! A prompt runs one turn of the same agent the Ask panel runs
//! ([`crate::agent::run`]) — the same read-only code tools, the same budget —
//! with the model configured in clew's settings (`config.toml`, or the
//! provider's key in the environment). Its tool calls are reported as
//! `tool_call` updates with the code locations they touched, its answer as
//! `agent_message_chunk`s, and the turn's history is replayed to the next
//! prompt of the session so follow-ups resolve.
//!
//! The MCP servers the editor hands over in `session/new` are connected, and
//! their tools offered to the turn too. Those tools may act on the world, so
//! each call asks the editor's user first (`session/request_permission`;
//! "always" is remembered for the session). `session/cancel` stops the turn.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use clew_core::fs_scan::{self, FileEntry};
use clew_core::linerpc::{RpcError, response};
use clew_core::{embed, llm, mcp};
use clew_protocol::{AiChatMsg, Event, ExternalTool, ServerMessage, StreamOutcome};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{self, PendingCalls};
use crate::agent_lsp::LspPool;

/// The ACP protocol version this agent speaks.
const PROTOCOL_VERSION: u64 = 1;

/// How old a session's file list may be before a turn rescans it.
const FILES_FRESH_FOR: Duration = Duration::from_secs(5);

/// How long the editor's MCP servers may take to connect at `session/new`.
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How many answered prompts a session replays to the next one.
const HISTORY_TURNS: usize = 6;

/// One editor tool of a session: where it runs.
#[derive(Clone)]
struct Tool {
    client: Arc<mcp::Client>,
    info: mcp::ToolInfo,
    server: String,
    timeout: Duration,
}

/// One session: a project and its conversation.
struct Session {
    root: PathBuf,
    files: Mutex<(Instant, Arc<Vec<FileEntry>>)>,
    lsp: Arc<LspPool>,
    history: Mutex<Vec<AiChatMsg>>,
    /// The running turn's stop flag.
    stop: Mutex<Option<Arc<AtomicBool>>>,
    /// The editor's MCP tools, by the name the model knows each by.
    tools: HashMap<String, Tool>,
    /// The tools the user allowed for the rest of the session.
    allowed: Mutex<HashSet<String>>,
    /// The last tool call's number: a `toolCallId` is the session's, unique
    /// in it across prompts (an editor finds a call's card by it).
    calls: AtomicU64,
}

impl Session {
    /// A `toolCallId` no other call of the session has.
    fn call_id(&self, kind: &str) -> String {
        format!(
            "clew-{kind}-{}",
            self.calls.fetch_add(1, Ordering::Relaxed) + 1
        )
    }

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

/// Requests of ours waiting for the editor's answer, by id.
type Waiting = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// What every task of the connection shares.
#[derive(Clone)]
struct Conn {
    out: mpsc::UnboundedSender<Value>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    waiting: Waiting,
    ids: Arc<AtomicI64>,
    /// The model to use instead of clew's configured one (tests).
    chat: Option<llm::Config>,
}

impl Conn {
    fn send(&self, msg: Value) {
        let _ = self.out.send(msg);
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn update(&self, session: &str, update: Value) {
        self.notify(
            "session/update",
            json!({ "sessionId": session, "update": update }),
        );
    }

    /// Ask the editor something, and wait for its answer (or for `stop`, or
    /// for as long as a turn waits for a tool).
    async fn ask(&self, method: &str, params: Value, stop: &AtomicBool) -> Result<Value, RpcError> {
        let deadline = Instant::now() + clew_protocol::EXTERNAL_TOOL_WAIT;
        let id = self.ids.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, mut rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            tokio::select! {
                answer = &mut rx => {
                    return answer.unwrap_or_else(|_| Err(RpcError::internal("the editor went away")));
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        self.waiting.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                        return Err(RpcError::internal("cancelled"));
                    }
                }
            }
        }
    }
}

/// Serve over this process's stdin and stdout until the editor closes its
/// end. Must run inside a tokio runtime.
pub async fn serve_acp() {
    serve(tokio::io::stdin(), tokio::io::stdout(), None).await;
}

/// Serve over `reader`/`writer`, with `chat` in place of clew's configured
/// model when given.
pub async fn serve<R, W>(reader: R, writer: W, chat: Option<llm::Config>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            let mut line = msg.to_string().into_bytes();
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                return;
            }
        }
    });
    let conn = Conn {
        out,
        sessions: Arc::default(),
        waiting: Arc::default(),
        ids: Arc::default(),
        chat,
    };
    let mut reader = BufReader::new(reader);
    let mut tasks = Vec::new();
    loop {
        let line = match clew_core::framing::read_json_line(&mut reader).await {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let messages = match msg {
            Value::Array(items) => items,
            msg => vec![msg],
        };
        for msg in messages {
            if let Some(task) = dispatch(&conn, msg) {
                tasks.push(task);
            }
        }
        tasks.retain(|t: &tokio::task::JoinHandle<()>| !t.is_finished());
    }
    // The editor is gone: stop every turn, and close the language servers.
    let sessions: Vec<Arc<Session>> = conn
        .sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain()
        .map(|(_, s)| s)
        .collect();
    for session in &sessions {
        if let Some(stop) = session
            .stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            stop.store(true, Ordering::Relaxed);
        }
    }
    for task in tasks {
        let _ = task.await;
    }
    for session in sessions {
        session.lsp.close();
        for tool in session.tools.values() {
            tool.client.close().await;
        }
    }
    drop(conn);
    let _ = writer_task.await;
}

/// Handle one message from the editor; long work runs on a task of its own.
fn dispatch(conn: &Conn, msg: Value) -> Option<tokio::task::JoinHandle<()>> {
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match (id, method) {
        // The editor's answer to one of our requests.
        (Some(id), None) => {
            let waiting = id.as_i64().and_then(|id| {
                conn.waiting
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id)
            });
            if let Some(tx) = waiting {
                let answer = match msg.get("error") {
                    Some(err) => Err(RpcError::new(
                        err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error"),
                    )),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(answer);
            }
            None
        }
        (None, Some(method)) => {
            if method == "session/cancel"
                && let Some(session) =
                    params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .and_then(|s| {
                            conn.sessions
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get(s)
                                .cloned()
                        })
                && let Some(stop) = session
                    .stop
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
            {
                stop.store(true, Ordering::Relaxed);
            }
            None
        }
        (Some(id), Some(method)) => {
            let reply = |result: Result<Value, RpcError>| conn.send(response(&id, result));
            match method.as_str() {
                "initialize" => {
                    reply(Ok(json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "agentCapabilities": {
                            "loadSession": false,
                            "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true },
                            "mcpCapabilities": { "http": true, "sse": false },
                        },
                        "agentInfo": { "name": "clew", "title": "Clew", "version": env!("CARGO_PKG_VERSION") },
                        "authMethods": [],
                    })));
                    None
                }
                "authenticate" => {
                    reply(Ok(json!({})));
                    None
                }
                "session/new" => {
                    let conn = conn.clone();
                    Some(tokio::spawn(async move {
                        let result = new_session(&conn, &params).await;
                        conn.send(response(&id, result));
                    }))
                }
                "session/prompt" => {
                    // The turn's stop flag is in place before anything else
                    // is read: a `session/cancel` right behind the prompt (or
                    // the editor leaving) stops it, however early.
                    let started = start_prompt(conn, &params);
                    let conn = conn.clone();
                    Some(tokio::spawn(async move {
                        let result = match started {
                            Ok((session_id, session, stop)) => {
                                let result =
                                    prompt(&conn, &session_id, &session, &stop, &params).await;
                                *session.stop.lock().unwrap_or_else(PoisonError::into_inner) = None;
                                result
                            }
                            Err(e) => Err(e),
                        };
                        conn.send(response(&id, result));
                    }))
                }
                other => {
                    reply(Err(RpcError::method_not_found(other)));
                    None
                }
            }
        }
        (None, None) => None,
    }
}

/// `session/new`: scan the project, connect the editor's MCP servers.
async fn new_session(conn: &Conn, params: &Value) -> Result<Value, RpcError> {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("session/new needs a cwd"))?;
    let root = Path::new(cwd)
        .canonicalize()
        .map_err(|e| RpcError::invalid_params(format!("{cwd}: {e}")))?;
    if !root.is_dir() {
        return Err(RpcError::invalid_params(format!("{cwd} is not a folder")));
    }
    let scan_root = root.clone();
    let files = tokio::task::spawn_blocking(move || fs_scan::scan(scan_root).files)
        .await
        .map_err(|_| RpcError::internal("the project scan failed"))?;
    let tools = connect_editor_servers(params.get("mcpServers")).await;
    let id = format!("clew-{}", conn.ids.fetch_add(1, Ordering::Relaxed) + 1);
    let session = Session {
        lsp: Arc::new(LspPool::new(root.clone(), Default::default())),
        root,
        files: Mutex::new((Instant::now(), Arc::new(files))),
        history: Mutex::new(Vec::new()),
        stop: Mutex::new(None),
        tools,
        allowed: Mutex::new(HashSet::new()),
        calls: AtomicU64::new(0),
    };
    conn.sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.clone(), Arc::new(session));
    Ok(json!({ "sessionId": id }))
}

/// The tools of the MCP servers the editor handed over; a server that does
/// not connect is left out.
async fn connect_editor_servers(servers: Option<&Value>) -> HashMap<String, Tool> {
    let pairs = |v: Option<&Value>| -> Vec<(String, String)> {
        v.and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|p| {
                        Some((
                            p.get("name")?.as_str()?.to_string(),
                            p.get("value")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let configs: Vec<mcp::ServerConfig> = servers
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = mcp::config::clean_name(s.get("name")?.as_str()?);
            let transport = match s.get("type").and_then(Value::as_str) {
                Some("http") => mcp::Transport::Http {
                    url: s.get("url")?.as_str()?.to_string(),
                    headers: pairs(s.get("headers")),
                },
                Some(_) => return None,
                None => mcp::Transport::Stdio(clew_core::linerpc::Launch {
                    program: s.get("command")?.as_str()?.to_string(),
                    args: s
                        .get("args")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    env: pairs(s.get("env")),
                    cwd: None,
                }),
            };
            Some(mcp::ServerConfig {
                name,
                transport,
                enabled: true,
                approve: mcp::Approve::Ask,
                allow: Vec::new(),
                timeout: mcp::config::DEFAULT_TIMEOUT,
            })
        })
        .collect();
    let mut tools = HashMap::new();
    for config in configs {
        let connected = tokio::time::timeout(MCP_CONNECT_TIMEOUT, async {
            let client = Arc::new(mcp::Client::connect(&config).await.ok()?);
            let list = client.tools().await.ok()?;
            Some((client, list))
        })
        .await
        .ok()
        .flatten();
        let Some((client, list)) = connected else {
            eprintln!(
                "[clew-server] acp: the MCP server `{}` did not connect",
                config.name
            );
            continue;
        };
        for info in list.iter() {
            let name = mcp::model_tool_name(&config.name, &info.name);
            tools.entry(name).or_insert_with(|| Tool {
                client: client.clone(),
                info: info.clone(),
                server: config.name.clone(),
                timeout: config.timeout,
            });
        }
    }
    tools
}

/// The text of a prompt: its text blocks, and the files it carries.
fn prompt_text(blocks: &[Value]) -> (String, String) {
    let mut question = String::new();
    let mut context = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if !question.is_empty() {
                    question.push('\n');
                }
                question.push_str(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            Some("resource") => {
                let res = block.get("resource");
                let uri = res
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(text) = res.and_then(|r| r.get("text")).and_then(Value::as_str) {
                    context.push_str(&format!("\n\nAttached `{uri}`:\n```\n{text}\n```"));
                }
            }
            Some("resource_link") => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                context.push_str(&format!("\n\nThe user points at `{uri}`."));
            }
            _ => {}
        }
    }
    (question, context.trim_start().to_string())
}

/// The ACP kind of one of clew's tools.
fn kind_of(tool: &str) -> &'static str {
    match tool {
        "search" | "semantic_find" | "references" | "definition" => "search",
        "read" | "outline" | "files" | "hover" | "explanations" | "history" | "changes" => "read",
        _ => "other",
    }
}

/// The session a `session/prompt` names, its turn's stop flag now in place.
fn start_prompt(
    conn: &Conn,
    params: &Value,
) -> Result<(String, Arc<Session>, Arc<AtomicBool>), RpcError> {
    let id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("no sessionId"))?
        .to_string();
    let session = conn
        .sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
        .ok_or_else(|| RpcError::invalid_params(format!("no session {id}")))?;
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut running = session.stop.lock().unwrap_or_else(PoisonError::into_inner);
        if running.is_some() {
            return Err(RpcError::internal(
                "a prompt is running in this session already",
            ));
        }
        *running = Some(stop.clone());
    }
    Ok((id, session, stop))
}

/// `session/prompt`: one agent turn. Every update it sends goes before its
/// answer — the editor's tool calls' included, stopped when the turn ends.
async fn prompt(
    conn: &Conn,
    id: &str,
    session: &Arc<Session>,
    stop: &Arc<AtomicBool>,
    params: &Value,
) -> Result<Value, RpcError> {
    if stop.load(Ordering::Relaxed) {
        return Ok(json!({ "stopReason": "cancelled" }));
    }
    let id = id.to_string();
    let blocks: Vec<Value> = params
        .get("prompt")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (question, context) = prompt_text(&blocks);
    let chat = conn
        .chat
        .clone()
        .or_else(llm::Config::load)
        .ok_or_else(|| {
            RpcError::internal(format!(
                "clew has no model to answer with — add an API key in clew's settings ({})",
                llm::config_hint()
            ))
        })?;
    let external: Vec<ExternalTool> = session
        .tools
        .iter()
        .map(|(name, tool)| ExternalTool {
            name: name.clone(),
            description: format!("[MCP server `{}`] {}", tool.server, tool.info.description),
            parameters: tool.info.input_schema.clone(),
        })
        .collect();
    let pending: PendingCalls = Default::default();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let budget = crate::OutputBudget::new();
    let history = session
        .history
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let turn = {
        let (session, stop, pending, budget) = (
            session.clone(),
            stop.clone(),
            pending.clone(),
            budget.clone(),
        );
        let rt = tokio::runtime::Handle::current();
        let question = question.clone();
        tokio::task::spawn_blocking(move || {
            agent::run(
                session.root.clone(),
                session.files(),
                chat,
                embed::Config::load(),
                session.lsp.clone(),
                rt,
                1,
                question,
                history,
                context,
                external,
                pending,
                &tx,
                &budget,
                &stop,
            );
        })
    };
    let mut answer = String::new();
    let mut tool_calls = tokio::task::JoinSet::new();
    let mut outcome = StreamOutcome::Failed("the turn ended without a word".into());
    while let Some(msg) = rx.recv().await {
        budget.release(crate::transport::bulk_weight(&msg));
        let ServerMessage::Notification { event } = msg else {
            continue;
        };
        match event {
            Event::AgentStep {
                tool, title, refs, ..
            } => {
                let locations: Vec<Value> = refs
                    .iter()
                    .map(|r| {
                        let mut loc =
                            json!({ "path": session.root.join(&r.rel).to_string_lossy() });
                        if let Some(line) = r.line {
                            loc["line"] = line.into();
                        }
                        loc
                    })
                    .collect();
                conn.update(
                    &id,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": session.call_id("step"),
                        "title": title,
                        "kind": kind_of(&tool),
                        "status": "completed",
                        "locations": locations,
                    }),
                );
            }
            Event::AgentDelta { text, .. } => {
                answer.push_str(&text);
                conn.update(
                    &id,
                    json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text },
                    }),
                );
            }
            Event::AgentToolCall {
                call, tool, args, ..
            } => {
                let conn = conn.clone();
                let (session, stop, pending, id) =
                    (session.clone(), stop.clone(), pending.clone(), id.clone());
                tool_calls.spawn(async move {
                    let (content, failed) =
                        run_editor_tool(&conn, &session, &id, &tool, args, &stop).await;
                    agent::deliver_tool_result(&pending, 1, call, content, failed);
                });
            }
            Event::AgentDone { outcome: done, .. } => {
                outcome = done;
                break;
            }
            _ => {}
        }
    }
    let _ = turn.await;
    // A call the turn no longer waits for (it gave up on it) is stopped —
    // its permission request withdrawn — and every call has said how it
    // ended before the answer goes.
    stop.store(true, Ordering::Relaxed);
    while tool_calls.join_next().await.is_some() {}
    match outcome {
        StreamOutcome::Done => {
            let mut history = session
                .history
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            history.push(AiChatMsg {
                role: "user".into(),
                content: question,
            });
            history.push(AiChatMsg {
                role: "assistant".into(),
                content: answer,
            });
            let keep = HISTORY_TURNS * 2;
            if history.len() > keep {
                let drop = history.len() - keep;
                history.drain(..drop);
            }
            Ok(json!({ "stopReason": "end_turn" }))
        }
        StreamOutcome::Stopped => Ok(json!({ "stopReason": "cancelled" })),
        StreamOutcome::Failed(why) => Err(RpcError::internal(why)),
    }
}

/// Run one of the editor's MCP tools for the turn — asking the editor's
/// user first, unless they allowed it for the session — and report it as a
/// tool call. Returns what the model is shown, and whether it failed.
async fn run_editor_tool(
    conn: &Conn,
    session: &Session,
    id: &str,
    name: &str,
    args: Value,
    stop: &AtomicBool,
) -> (String, bool) {
    let Some(tool) = session.tools.get(name).cloned() else {
        return (format!("`{name}` is not a tool of this session"), true);
    };
    let call_id = session.call_id("mcp");
    let title = format!("{} · {}", tool.server, tool.info.display_name());
    conn.update(
        id,
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call_id,
            "title": title,
            "kind": if tool.info.read_only { "read" } else { "other" },
            "status": "pending",
            "rawInput": args,
        }),
    );
    let allowed = session
        .allowed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(name);
    if !allowed {
        let asked = conn
            .ask(
                "session/request_permission",
                json!({
                    "sessionId": id,
                    "toolCall": { "toolCallId": call_id, "title": title, "rawInput": args },
                    "options": [
                        { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "allow_always", "name": "Always allow", "kind": "allow_always" },
                        { "optionId": "reject_once", "name": "Deny", "kind": "reject_once" },
                    ],
                }),
                stop,
            )
            .await;
        let chosen = asked
            .ok()
            .and_then(|r| r.get("outcome").cloned())
            .filter(|o| o.get("outcome").and_then(Value::as_str) == Some("selected"))
            .and_then(|o| o.get("optionId").and_then(Value::as_str).map(str::to_owned));
        match chosen.as_deref() {
            Some("allow_always") => {
                session
                    .allowed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(name.to_string());
            }
            Some("allow_once") => {}
            _ => {
                conn.update(
                    id,
                    json!({ "sessionUpdate": "tool_call_update", "toolCallId": call_id, "status": "failed" }),
                );
                return (
                    "The user declined to run this tool; go on without it.".into(),
                    true,
                );
            }
        }
    }
    conn.update(
        id,
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": call_id, "status": "in_progress" }),
    );
    let result = tokio::select! {
        result = tool.client.call_tool(&tool.info, args, tool.timeout) => result.map_err(|e| e.to_string()),
        () = async { while !stop.load(Ordering::Relaxed) { tokio::time::sleep(Duration::from_millis(100)).await } } => {
            Err("cancelled".into())
        }
    };
    let (content, failed) = match result {
        Ok(out) => (out.text, out.is_error),
        Err(why) => (format!("The MCP server failed: {why}"), true),
    };
    conn.update(
        id,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "status": if failed { "failed" } else { "completed" },
            "content": [{ "type": "content", "content": { "type": "text", "text": content.chars().take(2000).collect::<String>() } }],
        }),
    );
    (content, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_core::acp;
    use std::io::Write;

    /// A model endpoint (OpenAI dialect) answering each request with the
    /// next of `bodies`, an SSE stream.
    fn provider(bodies: Vec<String>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for (conn, body) in listener.incoming().zip(bodies) {
                let Ok(mut conn) = conn else { return };
                let _ = clew_core::testutil::read_http_within(
                    &mut conn,
                    clew_core::testutil::HTTP_READ_BOUND,
                );
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            }
        });
        base
    }

    fn sse(events: &[Value]) -> String {
        let mut body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        body
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_editor_asks_clew_through_acp() {
        let dir = clew_core::testutil::TempDir::new("acp-agent");
        std::fs::write(dir.path().join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        let base = provider(vec![
            sse(&[
                json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": "c0", "function": { "name": "search", "arguments": "{\"query\":\"answer\"}" }
            }] } }] }),
                json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            ]),
            sse(&[
                json!({ "choices": [{ "delta": { "content": "It returns 42." }, "finish_reason": "stop" }] }),
            ]),
        ]);
        let chat = llm::Config::from_parts(llm::Provider::Custom, "k".into(), "m".into(), base);
        let (client_out, server_in) = tokio::io::duplex(1 << 16);
        let (server_out, client_in) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(server_in, server_out, Some(chat)));
        // clew's own ACP client drives clew's ACP agent.
        let (agent, mut events) = acp::Agent::over(client_in, client_out, dir.path())
            .await
            .unwrap();
        assert_eq!(agent.info().display_name(), "Clew");
        let session = agent.new_session(&[]).await.unwrap();
        let agent = Arc::new(agent);
        let turn = {
            let (agent, session) = (agent.clone(), session.clone());
            tokio::spawn(async move {
                agent
                    .prompt(&session, vec![acp::text_block("what does answer return?")])
                    .await
            })
        };
        let mut text = String::new();
        let mut calls = Vec::new();
        while text != "It returns 42." {
            match tokio::time::timeout(Duration::from_secs(30), events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                acp::AgentEvent::Text { text: t, .. } => text.push_str(&t),
                acp::AgentEvent::ToolCall { call, .. } => calls.push(call),
                _ => {}
            }
        }
        assert_eq!(turn.await.unwrap().unwrap(), acp::StopReason::EndTurn);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].kind.as_deref(), Some("search"));
        assert_eq!(calls[0].status, Some(acp::ToolStatus::Completed));
        let (path, line) = &calls[0].locations.as_ref().unwrap()[0];
        assert!(
            path.ends_with("lib.rs") && *line == Some(1),
            "{path:?} {line:?}"
        );
        agent.stop().await;
    }

    /// The editor's end of a connection to [`serve`], speaking JSON lines.
    struct Editor {
        lines: tokio::io::Lines<BufReader<tokio::io::DuplexStream>>,
        out: tokio::io::DuplexStream,
    }

    impl Editor {
        fn start(chat: llm::Config) -> Editor {
            use tokio::io::AsyncBufReadExt;
            let (editor_out, server_in) = tokio::io::duplex(1 << 16);
            let (server_out, editor_in) = tokio::io::duplex(1 << 16);
            tokio::spawn(serve(server_in, server_out, Some(chat)));
            Editor {
                lines: BufReader::new(editor_in).lines(),
                out: editor_out,
            }
        }

        async fn send(&mut self, msgs: &[Value]) {
            let text: String = msgs.iter().map(|m| format!("{m}\n")).collect();
            self.out.write_all(text.as_bytes()).await.unwrap();
        }

        async fn recv(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
                .await
                .expect("clew answers")
                .unwrap()
                .expect("clew is still there");
            serde_json::from_str(&line).unwrap()
        }

        /// Initialize, and open a session on `dir` with `mcp_servers`.
        async fn open(&mut self, dir: &Path, mcp_servers: Value) -> String {
            self.send(&[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                    "params": {"cwd": dir.to_string_lossy(), "mcpServers": mcp_servers}}),
            ])
            .await;
            loop {
                let msg = self.recv().await;
                if msg["id"] == 2 {
                    return msg["result"]["sessionId"].as_str().unwrap().to_string();
                }
            }
        }
    }

    /// A model endpoint nothing listens on: a turn that reaches it fails.
    fn no_model() -> llm::Config {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        llm::Config::from_parts(
            llm::Provider::Custom,
            "k".into(),
            "m".into(),
            format!("http://127.0.0.1:{port}"),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_right_behind_the_prompt_stops_it() {
        let dir = clew_core::testutil::TempDir::new("acp-early-cancel");
        let mut editor = Editor::start(no_model());
        let session = editor.open(dir.path(), json!([])).await;
        // The cancel is read before the prompt's turn has begun.
        editor
            .send(&[
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                    "params": {"sessionId": session, "prompt": [{"type": "text", "text": "why?"}]}}),
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}),
            ])
            .await;
        let answer = loop {
            let msg = editor.recv().await;
            if msg["id"] == 3 {
                break msg;
            }
        };
        assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    }

    /// An MCP server over HTTP (modern era) with the one tool `poke`.
    fn poke_server() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                std::thread::spawn(move || {
                    let request = clew_core::testutil::read_http_within(
                        &mut conn,
                        clew_core::testutil::HTTP_READ_BOUND,
                    );
                    let msg: Value = serde_json::from_str(&request.body).unwrap_or_default();
                    let result = match msg["method"].as_str() {
                        Some("server/discover") => json!({"resultType": "complete",
                            "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}},
                            "serverInfo": {"name": "poke", "version": "1"}}),
                        Some("tools/list") => json!({"resultType": "complete", "tools": [
                            {"name": "poke", "description": "Pokes.", "inputSchema": {"type": "object"}}
                        ]}),
                        _ => {
                            json!({"resultType": "complete", "content": [{"type": "text", "text": "poked"}]})
                        }
                    };
                    let body =
                        json!({"jsonrpc": "2.0", "id": msg["id"], "result": result}).to_string();
                    let _ = write!(
                        conn,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        url
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_turn_withdraws_its_tool_calls_before_it_answers() {
        let dir = clew_core::testutil::TempDir::new("acp-withdraw");
        let base = provider(vec![sse(&[
            json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": "c0", "function": { "name": "mcp__web__poke", "arguments": "{}" }
            }] } }] }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
        ])]);
        let chat = llm::Config::from_parts(llm::Provider::Custom, "k".into(), "m".into(), base);
        let mut editor = Editor::start(chat);
        let session = editor
            .open(
                dir.path(),
                json!([{"type": "http", "name": "web", "url": poke_server(), "headers": []}]),
            )
            .await;
        editor
            .send(&[
                json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": session, "prompt": [{"type": "text", "text": "poke it"}]}}),
            ])
            .await;
        // The call asks for permission; the user never answers, and stops
        // the turn instead.
        let call_id = loop {
            let msg = editor.recv().await;
            if msg["method"] == "session/request_permission" {
                break msg["params"]["toolCall"]["toolCallId"].clone();
            }
        };
        editor
            .send(&[json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})])
            .await;
        let mut withdrawn = false;
        let answer = loop {
            let msg = editor.recv().await;
            let update = &msg["params"]["update"];
            if update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == call_id
                && update["status"] == "failed"
            {
                withdrawn = true;
            }
            if msg["id"] == 3 {
                break msg;
            }
        };
        assert!(withdrawn, "the call said it ended before the answer");
        assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tool_call_ids_stay_unique_across_prompts() {
        let dir = clew_core::testutil::TempDir::new("acp-ids");
        std::fs::write(dir.path().join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
        let search = || {
            sse(&[
                json!({ "choices": [{ "delta": { "tool_calls": [{
                    "index": 0, "id": "c0", "function": { "name": "search", "arguments": "{\"query\":\"answer\"}" }
                }] } }] }),
                json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            ])
        };
        let done = || {
            sse(&[
                json!({ "choices": [{ "delta": { "content": "42." }, "finish_reason": "stop" }] }),
            ])
        };
        let base = provider(vec![search(), done(), search(), done()]);
        let chat = llm::Config::from_parts(llm::Provider::Custom, "k".into(), "m".into(), base);
        let mut editor = Editor::start(chat);
        let session = editor.open(dir.path(), json!([])).await;
        let mut ids = Vec::new();
        for id in [3, 4] {
            editor
                .send(&[json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
                    "params": {"sessionId": session, "prompt": [{"type": "text", "text": "what?"}]}})])
                .await;
            loop {
                let msg = editor.recv().await;
                if msg["params"]["update"]["sessionUpdate"] == "tool_call" {
                    ids.push(
                        msg["params"]["update"]["toolCallId"]
                            .as_str()
                            .unwrap()
                            .to_string(),
                    );
                }
                if msg["id"] == id {
                    assert_eq!(msg["result"]["stopReason"], "end_turn", "{msg}");
                    break;
                }
            }
        }
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "each call of the session is its own");
    }

    #[test]
    fn prompts_read_their_text_and_files() {
        let (q, ctx) = prompt_text(&[
            json!({"type": "text", "text": "why?"}),
            json!({"type": "resource", "resource": {"uri": "file:///p/a.rs", "text": "fn a() {}"}}),
            json!({"type": "resource_link", "uri": "file:///p/b.rs", "name": "b.rs"}),
            json!({"type": "image", "data": "", "mimeType": "image/png"}),
        ]);
        assert_eq!(q, "why?");
        assert!(ctx.contains("Attached `file:///p/a.rs`") && ctx.contains("fn a() {}"));
        assert!(ctx.contains("`file:///p/b.rs`"));
    }
}
