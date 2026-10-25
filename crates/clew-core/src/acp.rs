//! The Agent Client Protocol (ACP, v1): clew as the CLIENT of a coding agent
//! the user configured ([`config`]) — Claude Code, Codex, Gemini CLI, any
//! agent that speaks ACP — so the Ask panel can hand a question to it.
//!
//! An [`Agent`] is the agent process, started in the project's folder and
//! spoken to over its stdio ([`crate::linerpc`]). `initialize` agrees on
//! protocol version 1 and says what this client offers: reading and writing
//! the project's text files (`fs/*`), and nothing else — no terminals, so the
//! agent runs commands with its own tools, under its own permission rules.
//! [`Agent::new_session`] opens a conversation in the project, handing the
//! agent MCP servers to use (clew's own code tools, from `clew-server mcp`,
//! and the user's servers when the agent is configured to share them);
//! [`Agent::prompt`] sends a turn and returns when the agent ends it, its
//! progress arriving meanwhile as [`AgentEvent`]s: message and thought text,
//! tool calls and their updates, the plan, and the permission requests the
//! user answers with [`Agent::answer_permission`]. [`Agent::cancel`] stops a
//! turn; every permission still open is then answered "cancelled", as the
//! protocol requires.
//!
//! The agent asks for files by absolute path. Only files inside the project
//! are served, read or written — the agent reaches anything else with its own
//! tools, as it would without clew — and every path is confined the way a
//! model-written path is ([`crate::confine`]): a link that leads out is
//! refused.

pub mod config;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::framing::CallError;
use crate::linerpc::{self, CancelStyle, PeerMsg, RpcError, Session};

pub use config::AgentConfig;

/// The protocol version this client speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// The error code an agent answers with when it needs the user signed in.
pub const AUTH_REQUIRED: i64 = -32000;

/// How long starting the agent and agreeing on the protocol may take.
/// Generous: `npx` may first download the agent.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long opening a session may take (the agent starts its MCP servers).
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a sign-in through the agent may take (it may open a browser).
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(300);

/// The longest one turn may run.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

/// Largest file the agent may read or write through clew.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// What the agent said about itself and what it supports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
    /// It can be handed MCP servers over HTTP.
    pub mcp_http: bool,
    /// Prompts may carry embedded file contents (`resource` blocks).
    pub embedded_context: bool,
    /// How the user may sign in, if the agent needs it.
    pub auth_methods: Vec<AuthMethod>,
}

impl AgentInfo {
    /// What to call the agent in front of a person.
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// A way to sign in to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `terminal`: the user runs the agent's own login in a terminal, which
    /// clew cannot do for them; `agent` (the default): the agent signs in
    /// when asked (`authenticate`).
    pub terminal: Option<Vec<String>>,
}

/// An MCP server handed to the agent in `session/new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerSpec {
    Stdio {
        name: String,
        command: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        name: String,
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl McpServerSpec {
    fn to_json(&self) -> Value {
        let pairs = |pairs: &[(String, String)]| -> Vec<Value> {
            pairs
                .iter()
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect()
        };
        match self {
            McpServerSpec::Stdio {
                name,
                command,
                args,
                env,
            } => json!({
                "name": name,
                "command": command.to_string_lossy(),
                "args": args,
                "env": pairs(env),
            }),
            McpServerSpec::Http { name, url, headers } => json!({
                "type": "http",
                "name": name,
                "url": url,
                "headers": pairs(headers),
            }),
        }
    }
}

/// How a tool call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    fn parse(s: &str) -> Option<ToolStatus> {
        Some(match s {
            "pending" => ToolStatus::Pending,
            "in_progress" => ToolStatus::InProgress,
            "completed" => ToolStatus::Completed,
            "failed" => ToolStatus::Failed,
            _ => return None,
        })
    }
}

/// What a tool call is, as far as the agent said (`tool_call`), or what
/// changed about it (`tool_call_update`: only the fields it names are set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallInfo {
    pub id: String,
    pub title: Option<String>,
    /// `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`,
    /// `fetch`, `switch_mode`, `other`.
    pub kind: Option<String>,
    pub status: Option<ToolStatus>,
    /// The files it touches, with a line when it names one.
    pub locations: Option<Vec<(PathBuf, Option<u32>)>>,
    /// A one-line account of what it produced: its text, or the files it
    /// changed.
    pub summary: Option<String>,
}

/// A setting of a session the agent lets the user choose — its mode, its
/// model, its reasoning level — with the values on offer. An agent's legacy
/// session modes are one of these too, under the id [`MODE_OPTION`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    /// `mode`, `model`, `thought_level`, … when the agent said.
    pub category: Option<String>,
    /// The value chosen now.
    pub current: String,
    /// The values on offer: `(value, name)`.
    pub choices: Vec<(String, String)>,
}

/// The id a session's legacy modes (`modes`, `session/set_mode`) go by
/// among its [`ConfigOption`]s.
pub const MODE_OPTION: &str = "_clew_mode";

/// The select options of a `configOptions` list (boolean ones are not
/// advertised, so not sent).
fn read_config_options(v: Option<&Value>) -> Vec<ConfigOption> {
    let Some(items) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("select"))
        .filter_map(|o| {
            let mut choices = Vec::new();
            for entry in o
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                // Grouped: `{group, name, options: [...]}`; ungrouped: the options.
                match entry.get("options").and_then(Value::as_array) {
                    Some(group) => choices.extend(group.iter().filter_map(read_choice)),
                    None => choices.extend(read_choice(entry)),
                }
            }
            Some(ConfigOption {
                id: o.get("id")?.as_str()?.to_string(),
                name: o
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                category: o.get("category").and_then(Value::as_str).map(str::to_owned),
                current: o
                    .get("currentValue")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                choices,
            })
        })
        .filter(|o| !o.choices.is_empty())
        .collect()
}

fn read_choice(v: &Value) -> Option<(String, String)> {
    let value = v.get("value")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&value)
        .to_string();
    Some((value, name))
}

/// A session's legacy modes as a [`ConfigOption`].
fn read_modes(v: Option<&Value>) -> Option<ConfigOption> {
    let modes = v?;
    let choices: Vec<(String, String)> = modes
        .get("availableModes")?
        .as_array()?
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            Some((id, name))
        })
        .collect();
    (!choices.is_empty()).then(|| ConfigOption {
        id: MODE_OPTION.into(),
        name: "Mode".into(),
        category: Some("mode".into()),
        current: modes
            .get("currentModeId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        choices,
    })
}

/// The options a `session/new` (or `set_config_option`) answer carries:
/// its config options, else its legacy modes.
fn read_session_options(result: &Value) -> Vec<ConfigOption> {
    let options = read_config_options(result.get("configOptions"));
    if !options.is_empty() {
        return options;
    }
    read_modes(result.get("modes")).into_iter().collect()
}

/// One entry of the agent's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    /// `pending`, `in_progress` or `completed`.
    pub status: String,
}

/// One option of a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// The agent asks the user before it does something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// The request's id, for [`Agent::answer_permission`].
    pub id: u64,
    pub session: String,
    pub tool: ToolCallInfo,
    pub options: Vec<PermissionOption>,
}

/// What a running agent reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A piece of the agent's answer.
    Text { session: String, text: String },
    /// A piece of its reasoning.
    Thought { session: String, text: String },
    /// A new tool call.
    ToolCall { session: String, call: ToolCallInfo },
    /// A change to one.
    ToolCallUpdate { session: String, call: ToolCallInfo },
    /// The plan, whole.
    Plan {
        session: String,
        entries: Vec<PlanEntry>,
    },
    /// The agent asks for permission.
    Permission(PermissionRequest),
    /// The agent wrote a file of the project through clew.
    FileWritten { path: PathBuf },
    /// The session's settings changed (the agent changed them, or a choice
    /// of the user's took effect): all of them, as they are now.
    Options {
        session: String,
        options: Vec<ConfigOption>,
    },
    /// The agent's process ended; the text says how.
    Exited(String),
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

impl StopReason {
    fn parse(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "max_turn_requests" => StopReason::MaxTurnRequests,
            "refusal" => StopReason::Refusal,
            "cancelled" => StopReason::Cancelled,
            other => StopReason::Other(other.to_string()),
        }
    }
}

/// Why talking to the agent failed.
#[derive(Debug, Clone, PartialEq)]
pub enum AcpError {
    /// The agent could not be started, or its process ended.
    Transport(String),
    TimedOut,
    /// The agent answered with an error.
    Rpc {
        code: i64,
        message: String,
    },
    /// The agent needs the user signed in, and cannot do it itself.
    NeedsLogin(String),
    /// The agent's answer is not what the protocol says.
    Protocol(String),
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::Transport(why) => write!(f, "{why}"),
            AcpError::TimedOut => write!(f, "the agent did not answer in time"),
            AcpError::Rpc { message, .. } => write!(f, "{message}"),
            AcpError::NeedsLogin(how) => write!(f, "the agent needs you to sign in: {how}"),
            AcpError::Protocol(why) => write!(f, "the agent's answer is not valid ACP: {why}"),
        }
    }
}

impl std::error::Error for AcpError {}

impl From<CallError> for AcpError {
    fn from(e: CallError) -> Self {
        match e {
            CallError::TimedOut => AcpError::TimedOut,
            CallError::NotRunning | CallError::Closed => {
                AcpError::Transport("the agent is not running".into())
            }
            CallError::Failed(f) => match f.code {
                Some(code) => AcpError::Rpc {
                    code,
                    message: f.message,
                },
                None => AcpError::Transport(format!("the agent {}", f.message)),
            },
        }
    }
}

/// The permission requests waiting for the user: our number → the agent's
/// request id and its session.
type OpenPermissions = Arc<Mutex<HashMap<u64, (Value, String)>>>;

/// Each session's settings, as the agent last described them.
type SessionOptions = Arc<Mutex<HashMap<String, Vec<ConfigOption>>>>;

/// A running agent.
pub struct Agent {
    session: Session,
    info: AgentInfo,
    root: PathBuf,
    permissions: OpenPermissions,
    options: SessionOptions,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.info.name)
            .field("alive", &self.alive())
            .finish()
    }
}

/// What the client tells the agent it can do.
fn client_capabilities() -> Value {
    json!({
        "fs": { "readTextFile": true, "writeTextFile": true },
        "terminal": false,
    })
}

impl Agent {
    /// Start the agent `config` names in the project at `root` and agree on
    /// the protocol with it. Its events arrive on the returned receiver for
    /// as long as it runs (the last one is [`AgentEvent::Exited`]).
    pub async fn start(
        config: &AgentConfig,
        root: &Path,
    ) -> Result<(Agent, mpsc::UnboundedReceiver<AgentEvent>), AcpError> {
        // The agent runs where its configuration says, else in a neutral
        // folder (not the project: see `linerpc::neutral_dir`); the project
        // is the session's `cwd`.
        let session = Session::spawn(&config.launch, options()).map_err(AcpError::Transport)?;
        Agent::open(session, root).await
    }

    /// An agent whose output is `reader` and input `writer` (tests).
    pub async fn over<R, W>(
        reader: R,
        writer: W,
        root: &Path,
    ) -> Result<(Agent, mpsc::UnboundedReceiver<AgentEvent>), AcpError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Agent::open(Session::over(reader, writer, options()), root).await
    }

    async fn open(
        session: Session,
        root: &Path,
    ) -> Result<(Agent, mpsc::UnboundedReceiver<AgentEvent>), AcpError> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let peer = session.take_peer().expect("a fresh session");
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let permissions: OpenPermissions = Arc::default();
        let options: SessionOptions = Arc::default();
        let result = session
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": client_capabilities(),
                    "clientInfo": crate::mcp::client_info(),
                }),
                INITIALIZE_TIMEOUT,
            )
            .await
            .map_err(|e| died(&session, AcpError::from(e)))?;
        let version = result.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(PROTOCOL_VERSION) {
            return Err(AcpError::Protocol(format!(
                "the agent speaks ACP version {}, and clew speaks {PROTOCOL_VERSION}",
                version.map_or("(none named)".to_string(), |v| v.to_string())
            )));
        }
        let info = read_info(&result);
        tokio::spawn(serve_peer(
            peer,
            session.handle(),
            root.clone(),
            events_tx,
            permissions.clone(),
            options.clone(),
            session.on_end(),
        ));
        Ok((
            Agent {
                session,
                info,
                root,
                permissions,
                options,
            },
            events_rx,
        ))
    }

    pub fn info(&self) -> &AgentInfo {
        &self.info
    }

    pub fn alive(&self) -> bool {
        self.session.alive()
    }

    /// What the agent process last wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        self.session.stderr_tail()
    }

    /// Open a conversation in the project, with `mcp` servers for the agent
    /// to use (an HTTP one only if the agent takes those). When the agent
    /// needs the user signed in and can do it itself, it is asked to, once.
    pub async fn new_session(&self, mcp: &[McpServerSpec]) -> Result<String, AcpError> {
        let servers: Vec<Value> = mcp
            .iter()
            .filter(|s| self.info.mcp_http || matches!(s, McpServerSpec::Stdio { .. }))
            .map(McpServerSpec::to_json)
            .collect();
        let params = json!({
            "cwd": self.root.to_string_lossy(),
            "mcpServers": servers,
        });
        let opened = match self
            .session
            .call("session/new", params.clone(), SESSION_TIMEOUT)
            .await
        {
            Err(CallError::Failed(f)) if f.code == Some(AUTH_REQUIRED) => {
                self.sign_in().await?;
                self.session
                    .call("session/new", params, SESSION_TIMEOUT)
                    .await
            }
            other => other,
        }
        .map_err(|e| died(&self.session, AcpError::from(e)))?;
        let id = opened
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| AcpError::Protocol("session/new named no sessionId".into()))?;
        self.options
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), read_session_options(&opened));
        Ok(id)
    }

    /// The settings of `session` the user may choose, as the agent last
    /// described them.
    pub fn options(&self, session: &str) -> Vec<ConfigOption> {
        self.options
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session)
            .cloned()
            .unwrap_or_default()
    }

    /// Choose `value` for setting `option` of `session`; the settings as
    /// they are afterwards.
    pub async fn set_option(
        &self,
        session: &str,
        option: &str,
        value: &str,
    ) -> Result<Vec<ConfigOption>, AcpError> {
        let answer = if option == MODE_OPTION {
            self.session
                .call(
                    "session/set_mode",
                    json!({ "sessionId": session, "modeId": value }),
                    SESSION_TIMEOUT,
                )
                .await?
        } else {
            self.session
                .call(
                    "session/set_config_option",
                    json!({ "sessionId": session, "configId": option, "value": value }),
                    SESSION_TIMEOUT,
                )
                .await?
        };
        let mut options = self.options.lock().unwrap_or_else(PoisonError::into_inner);
        let list = options.entry(session.to_string()).or_default();
        let fresh = read_config_options(answer.get("configOptions"));
        if !fresh.is_empty() {
            *list = fresh;
        } else if let Some(o) = list.iter_mut().find(|o| o.id == option) {
            o.current = value.to_string();
        }
        Ok(list.clone())
    }

    /// Ask the agent to sign the user in with the first method it can run
    /// itself; or, when it has only terminal logins, say which.
    async fn sign_in(&self) -> Result<(), AcpError> {
        let methods = &self.info.auth_methods;
        if let Some(method) = methods.iter().find(|m| m.terminal.is_none()) {
            self.session
                .call(
                    "authenticate",
                    json!({ "methodId": method.id }),
                    AUTH_TIMEOUT,
                )
                .await?;
            return Ok(());
        }
        Err(AcpError::NeedsLogin(match methods.first() {
            Some(AuthMethod {
                terminal: Some(args),
                name,
                ..
            }) => format!(
                "{name} — run the agent's command with {} in a terminal, then ask again",
                if args.is_empty() {
                    "no arguments".to_string()
                } else {
                    args.join(" ")
                }
            ),
            _ => "sign in with the agent's own command in a terminal, then ask again".into(),
        }))
    }

    /// Send one turn and wait for the agent to end it. `prompt` is the
    /// user's message as content blocks ([`text_block`], [`file_block`]).
    pub async fn prompt(&self, session: &str, prompt: Vec<Value>) -> Result<StopReason, AcpError> {
        let result = self
            .session
            .call(
                "session/prompt",
                json!({ "sessionId": session, "prompt": prompt }),
                PROMPT_TIMEOUT,
            )
            .await
            .map_err(|e| died(&self.session, AcpError::from(e)))?;
        Ok(StopReason::parse(
            result
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn"),
        ))
    }

    /// Stop the turn running in `session`, and answer every permission
    /// request of it still open with "cancelled".
    pub fn cancel(&self, session: &str) {
        self.session
            .notify("session/cancel", json!({ "sessionId": session }));
        let open: Vec<(Value, String)> = {
            let mut perms = self
                .permissions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let ids: Vec<u64> = perms
                .iter()
                .filter(|(_, (_, s))| s == session)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter().filter_map(|id| perms.remove(&id)).collect()
        };
        for (id, _) in open {
            self.session
                .respond(&id, Ok(json!({ "outcome": { "outcome": "cancelled" } })));
        }
    }

    /// Answer permission request `id` with the option the user chose, or
    /// `None` for "cancelled". An answer for a request no longer open is
    /// dropped.
    pub fn answer_permission(&self, id: u64, option: Option<&str>) {
        let Some((request, _)) = self
            .permissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
        else {
            return;
        };
        let outcome = match option {
            Some(option) => json!({ "outcome": "selected", "optionId": option }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.session
            .respond(&request, Ok(json!({ "outcome": outcome })));
    }

    /// End the agent: its input closed, then its process group killed after
    /// a grace.
    pub async fn stop(&self) {
        self.session.stop().await;
    }
}

/// A text content block.
pub fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// A file of the project as a content block: its contents embedded when
/// the agent takes them, else a link to it.
pub fn file_block(info: &AgentInfo, path: &Path, text: Option<&str>) -> Value {
    let uri = format!("file://{}", path.to_string_lossy());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match text {
        Some(text) if info.embedded_context => json!({
            "type": "resource",
            "resource": { "uri": uri, "text": text },
        }),
        _ => json!({ "type": "resource_link", "uri": uri, "name": name }),
    }
}

fn options() -> linerpc::Options {
    linerpc::Options {
        quick: Box::new(|_, _| None),
        cancel: Some(CancelStyle::Acp),
    }
}

/// `e`, with what the agent wrote to stderr when it has died.
fn died(session: &Session, e: AcpError) -> AcpError {
    match session.stderr_tail() {
        tail if tail.is_empty() || session.alive() => e,
        tail => AcpError::Transport(format!("{e}; it said:\n{tail}")),
    }
}

fn read_info(result: &Value) -> AgentInfo {
    let caps = result.get("agentCapabilities");
    let flag = |path: &[&str]| {
        let mut v = caps;
        for key in path {
            v = v.and_then(|v| v.get(key));
        }
        v.and_then(Value::as_bool).unwrap_or(false)
    };
    let info = result.get("agentInfo");
    let text = |v: Option<&Value>, key: &str| {
        v.and_then(|v| v.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let auth_methods = result
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|methods| {
            methods
                .iter()
                .filter_map(|m| {
                    let id = text(Some(m), "id")?;
                    let terminal = (m.get("type").and_then(Value::as_str) == Some("terminal"))
                        .then(|| {
                            m.get("args")
                                .and_then(Value::as_array)
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                })
                                .unwrap_or_default()
                        });
                    Some(AuthMethod {
                        name: text(Some(m), "name").unwrap_or_else(|| id.clone()),
                        description: text(Some(m), "description"),
                        id,
                        terminal,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    AgentInfo {
        name: text(info, "name").unwrap_or_else(|| "agent".into()),
        title: text(info, "title"),
        version: text(info, "version").unwrap_or_default(),
        mcp_http: flag(&["mcpCapabilities", "http"]),
        embedded_context: flag(&["promptCapabilities", "embeddedContext"]),
        auth_methods,
    }
}

/// The agent's own messages: session updates turned into events, permission
/// requests handed to the user, file reads and writes served from the
/// project, and everything else refused.
async fn serve_peer(
    mut peer: mpsc::UnboundedReceiver<PeerMsg>,
    handle: crate::framing::Handle,
    root: PathBuf,
    events: mpsc::UnboundedSender<AgentEvent>,
    permissions: OpenPermissions,
    options: SessionOptions,
    ended: impl Future<Output = String> + Send + 'static,
) {
    let respond = |id: &Value, result: Result<Value, RpcError>| {
        handle.send_raw(linerpc::response(id, result));
    };
    let mut next_permission = 0u64;
    while let Some(msg) = peer.recv().await {
        match msg {
            PeerMsg::Notification { method, params } => {
                if method != "session/update" {
                    continue;
                }
                if let Some(changed) = options_update(&params, &options) {
                    let _ = events.send(changed);
                } else if let Some(event) = read_update(&params) {
                    let _ = events.send(event);
                }
            }
            PeerMsg::Request { id, method, params } => match method.as_str() {
                "session/request_permission" => {
                    next_permission += 1;
                    let session = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let request = PermissionRequest {
                        id: next_permission,
                        session: session.clone(),
                        tool: params
                            .get("toolCall")
                            .map(read_tool_call)
                            .unwrap_or_default(),
                        options: read_options(params.get("options")),
                    };
                    if request.options.is_empty() {
                        respond(&id, Ok(json!({ "outcome": { "outcome": "cancelled" } })));
                        continue;
                    }
                    permissions
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(next_permission, (id, session));
                    if events.send(AgentEvent::Permission(request)).is_err() {
                        // Nobody to ask: the request stays open until the
                        // agent cancels its turn.
                    }
                }
                "fs/read_text_file" => {
                    let root = root.clone();
                    let answer = tokio::task::spawn_blocking(move || read_file(&root, &params))
                        .await
                        .unwrap_or_else(|_| Err(RpcError::internal("the read failed")));
                    respond(&id, answer);
                }
                "fs/write_text_file" => {
                    let root = root.clone();
                    let written =
                        tokio::task::spawn_blocking(move || write_file(&root, &params)).await;
                    match written {
                        Ok(Ok(path)) => {
                            respond(&id, Ok(json!({})));
                            let _ = events.send(AgentEvent::FileWritten { path });
                        }
                        Ok(Err(e)) => respond(&id, Err(e)),
                        Err(_) => respond(&id, Err(RpcError::internal("the write failed"))),
                    }
                }
                other => respond(&id, Err(RpcError::method_not_found(other))),
            },
        }
    }
    let _ = events.send(AgentEvent::Exited(ended.await));
}

/// A `config_option_update` or `current_mode_update`: the session's stored
/// settings changed, as an [`AgentEvent::Options`].
fn options_update(params: &Value, options: &SessionOptions) -> Option<AgentEvent> {
    let session = params.get("sessionId")?.as_str()?.to_string();
    let update = params.get("update")?;
    let mut all = options.lock().unwrap_or_else(PoisonError::into_inner);
    let list = all.entry(session.clone()).or_default();
    match update.get("sessionUpdate")?.as_str()? {
        "config_option_update" => {
            *list = read_config_options(update.get("configOptions"));
        }
        "current_mode_update" => {
            let mode = update.get("currentModeId")?.as_str()?;
            if let Some(o) = list.iter_mut().find(|o| o.id == MODE_OPTION) {
                o.current = mode.to_string();
            }
        }
        _ => return None,
    }
    Some(AgentEvent::Options {
        session,
        options: list.clone(),
    })
}

/// A `session/update` notification as an event, if it is one clew shows.
pub fn read_update(params: &Value) -> Option<AgentEvent> {
    let session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    Some(match kind {
        "agent_message_chunk" => AgentEvent::Text {
            session,
            text: block_text(update.get("content")?),
        },
        "agent_thought_chunk" => AgentEvent::Thought {
            session,
            text: block_text(update.get("content")?),
        },
        "tool_call" => AgentEvent::ToolCall {
            session,
            call: read_tool_call(update),
        },
        "tool_call_update" => AgentEvent::ToolCallUpdate {
            session,
            call: read_tool_call(update),
        },
        "plan" => AgentEvent::Plan {
            session,
            entries: update
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|e| {
                            Some(PlanEntry {
                                content: e.get("content")?.as_str()?.to_string(),
                                status: e
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending")
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        _ => return None,
    })
}

/// A content block as text: its text, or a line naming what it is.
fn block_text(block: &Value) -> String {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("resource_link") => format!(
            "[{}]({})",
            block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("resource"),
            block.get("uri").and_then(Value::as_str).unwrap_or_default()
        ),
        Some("resource") => block
            .get("resource")
            .and_then(|r| r.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("image") => "[an image]".into(),
        Some("audio") => "[audio]".into(),
        _ => String::new(),
    }
}

/// A `tool_call` or `tool_call_update` (or a permission request's tool).
fn read_tool_call(v: &Value) -> ToolCallInfo {
    let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    let locations = v.get("locations").and_then(Value::as_array).map(|locs| {
        locs.iter()
            .filter_map(|l| {
                let path = l.get("path")?.as_str()?;
                let line = l
                    .get("line")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok());
                Some((PathBuf::from(path), line))
            })
            .collect()
    });
    let summary = v.get("content").and_then(Value::as_array).map(|items| {
        let parts: Vec<String> = items
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("diff") => {
                    let path = item.get("path").and_then(Value::as_str).unwrap_or("a file");
                    let old = item
                        .get("oldText")
                        .and_then(Value::as_str)
                        .map_or(0, |t| t.lines().count());
                    let new = item
                        .get("newText")
                        .and_then(Value::as_str)
                        .map_or(0, |t| t.lines().count());
                    Some(format!("edited {path} (−{old} +{new} lines)"))
                }
                Some("content") => {
                    let text = block_text(item.get("content")?);
                    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
                    Some(line.chars().take(160).collect())
                }
                Some("terminal") => Some("terminal output".into()),
                _ => None,
            })
            .collect();
        parts.join("; ")
    });
    ToolCallInfo {
        id: text("toolCallId").unwrap_or_default(),
        title: text("title"),
        kind: text("kind"),
        status: v
            .get("status")
            .and_then(Value::as_str)
            .and_then(ToolStatus::parse),
        locations,
        summary: summary.filter(|s| !s.is_empty()),
    }
}

fn read_options(v: Option<&Value>) -> Vec<PermissionOption> {
    v.and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    Some(PermissionOption {
                        id: o.get("optionId")?.as_str()?.to_string(),
                        name: o
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        kind: o
                            .get("kind")
                            .and_then(Value::as_str)
                            .unwrap_or("allow_once")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The project-relative path of an absolute `path` the agent named, or why
/// it is not one of the project's.
fn project_rel(root: &Path, path: &str) -> Result<String, RpcError> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(RpcError::invalid_params("the path must be absolute"));
    }
    let rel = path.strip_prefix(root).map_err(|_| {
        RpcError::invalid_params(format!(
            "{} is outside the project — clew serves only the project's files",
            path.display()
        ))
    })?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    crate::confine::check_lexical(&rel)
        .map_err(|e| RpcError::invalid_params(format!("{}: {e}", path.display())))?;
    Ok(rel)
}

/// `fs/read_text_file`: a text file of the project, or the lines `line`
/// (1-based) on of it, at most `limit` of them.
fn read_file(root: &Path, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("no path"))?;
    let rel = project_rel(root, path)?;
    let abs = crate::confine::confine(root, &rel)
        .map_err(|e| RpcError::invalid_params(format!("{path}: {e}")))?;
    let text = match crate::fs_scan::read_confined_capped_checked(root, &abs, MAX_FILE_BYTES) {
        Ok(Some(text)) => text,
        Ok(None) => return Err(RpcError::new(-32002, format!("{path} does not exist"))),
        Err(e) => return Err(RpcError::invalid_params(format!("{path}: {e}"))),
    };
    let line = params
        .get("line")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1) as usize;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let content = if line == 1 && limit.is_none() {
        text
    } else {
        let lines = text.split_inclusive('\n').skip(line - 1);
        match limit {
            Some(n) => lines.take(n).collect(),
            None => lines.collect(),
        }
    };
    Ok(json!({ "content": content }))
}

/// `fs/write_text_file`: write a text file of the project — a new one in a
/// folder of the project, or a regular file that is there (never through a
/// link). Returns the path written.
/// The project's folder `dir_rel` (canonical), its missing folders made one
/// at a time, each in a folder already found inside the project — so a link
/// on the way that leads out is refused before anything is made through it.
fn make_dirs(root: &Path, dir_rel: &str) -> Result<PathBuf, String> {
    let mut dir = root.canonicalize().map_err(|e| e.to_string())?;
    let mut so_far = String::new();
    for part in dir_rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if !so_far.is_empty() {
            so_far.push('/');
        }
        so_far.push_str(part);
        let next = dir.join(part);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_dir() => dir = next,
            Ok(meta) if meta.file_type().is_symlink() => {
                let to = crate::confine::confine(root, &so_far).map_err(|e| e.to_string())?;
                if !to.is_dir() {
                    return Err(format!("{so_far} is not a folder"));
                }
                dir = to;
            }
            Ok(_) => return Err(format!("{so_far} is not a folder")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&next).map_err(|e| format!("{so_far}: {e}"))?;
                dir = next;
            }
            Err(e) => return Err(format!("{so_far}: {e}")),
        }
    }
    Ok(dir)
}

fn write_file(root: &Path, params: &Value) -> Result<PathBuf, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("no path"))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("no content"))?;
    if content.len() as u64 > MAX_FILE_BYTES {
        return Err(RpcError::invalid_params("the content is too large"));
    }
    let rel = project_rel(root, path)?;
    let (dir_rel, name) = match rel.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, rel.as_str()),
    };
    let dir = match dir_rel {
        Some(dir_rel) => make_dirs(root, dir_rel)
            .map_err(|e| RpcError::invalid_params(format!("{path}: {e}")))?,
        None => root
            .canonicalize()
            .map_err(|e| RpcError::internal(format!("{}: {e}", root.display())))?,
    };
    let target = dir.join(name);
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(RpcError::invalid_params(format!(
                "{path} is not a regular file"
            )));
        }
        _ => {}
    }
    crate::statefile::write_atomic(&target, content.as_bytes())
        .map_err(|e| RpcError::internal(format!("{path}: {e}")))?;
    Ok(target)
}

#[cfg(test)]
mod tests;
