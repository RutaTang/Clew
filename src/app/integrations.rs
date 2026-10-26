//! MCP and ACP in the app.
//!
//! **MCP** — the user's MCP servers lend their tools to clew's own Ask agent.
//! When a question is asked, the configured servers are reached (once per
//! process, [`mcp::hub::Hub`]) and their tools listed; the agent turn is
//! started with them ([`clew_protocol::ExternalTool`]); a call the turn makes
//! comes back as `AgentToolCall`, runs HERE — after the user approved it,
//! unless the server's policy says it need not ask — and its result goes back
//! to the turn as `AgentToolResult`. Each call is a step chip; a call waiting
//! for approval is a card under the turn.
//!
//! **ACP** — the question goes to a coding agent instead ([`acp::Agent`]):
//! started on first use in the project's folder, with clew's own code tools
//! (`clew-server mcp`) — and, when the agent is set to share them, the
//! user's MCP servers — for it to use. Its answer, reasoning, plan and tool
//! calls stream into the turn; its permission requests become approval cards
//! with the options it offered. ACP agents run on this machine, so they work
//! on local projects.
//!
//! **Settings** — the modal's MCP and ACP section lists, adds and removes
//! entries; every change is written to config.toml at once.

use crate::app::prelude::*;
use crate::*;

use clew_core::acp;
use clew_core::mcp;
use clew_protocol::ExternalTool;
use iced::futures::SinkExt;

/// How long a question waits for its MCP servers before it is asked without
/// the ones that are not ready.
const GATHER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Most characters of one string among a tool call's arguments an approval
/// card shows (the rest is counted, not shown): every argument stays in view
/// however long another is.
const MAX_ARG_CHARS: usize = 600;

/// Most characters of a tool call's arguments an approval card shows in all;
/// past it the card says how much it left out.
const MAX_DETAIL_CHARS: usize = 20_000;

/// How long the next question waits for an ACP agent to end a turn the user
/// stopped before the agent is started afresh for it.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Most characters of an ACP agent's reasoning kept for the turn.
const MAX_THOUGHT_CHARS: usize = 1_500;

/// One MCP tool as an agent turn was offered it.
#[derive(Debug, Clone)]
pub struct ToolTarget {
    pub server: mcp::ServerConfig,
    pub client: Arc<mcp::Client>,
    pub tool: mcp::ToolInfo,
}

/// A turn's MCP tools, by the name the model knows each by.
pub type ToolMap = HashMap<String, ToolTarget>;

/// An MCP tool call waiting for the user's approval.
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub target: ToolTarget,
    /// The arguments the model gave it.
    pub args: serde_json::Value,
    /// When the turn stops waiting for its result ([`call_deadline`]).
    pub deadline: std::time::Instant,
}

/// When the turn that asks for a tool call now stops waiting for its result:
/// the contract's wait, less a margin for the result's way back.
pub(crate) fn call_deadline() -> std::time::Instant {
    std::time::Instant::now() + clew_protocol::EXTERNAL_TOOL_WAIT
        - std::time::Duration::from_secs(15)
}

/// The ACP agent a project's questions go to.
#[derive(Debug)]
pub struct AcpLink {
    /// The configured name.
    pub name: String,
    /// What to call it in the panel.
    pub label: String,
    /// The running agent, once it started.
    pub agent: Option<Arc<acp::Agent>>,
    /// The conversation open with it.
    pub session: Option<String>,
    /// The turn running, by stream id.
    pub turn: Option<u64>,
    /// A question waiting for the agent (or a session) to be ready.
    pub pending: Option<(u64, Vec<serde_json::Value>)>,
    /// The `session/new` in flight: abort-on-drop, so dropping the link lets
    /// the agent go at once.
    pub opening: Option<iced::task::Handle>,
    /// A turn the user stopped that the agent has not ended yet: the next
    /// question waits for it, as the agent may refuse a prompt while one runs.
    pub cancelling: Option<u64>,
    /// The task that starts the agent and forwards its events: abort-on-drop,
    /// so dropping the link stops the forwarding and lets the agent go.
    pub pump: Option<iced::task::Handle>,
    /// The running turn's `session/prompt`.
    pub prompt: Option<iced::task::Handle>,
    /// The session's settings the user may choose (its mode, its model…).
    pub options: Vec<acp::ConfigOption>,
    /// The decline clew added to each permission request that offered
    /// none, by request: its option id (`with_a_decline`).
    pub declines: HashMap<u64, String>,
    /// The user's choices among them, `(option, value)`: a new session with
    /// the agent (after Clear, or a restart) starts with them too.
    pub chosen: Vec<(String, String)>,
}

impl AcpLink {
    /// A link to the agent `name`, not started yet.
    pub fn new(name: String, label: String) -> AcpLink {
        AcpLink {
            name,
            label,
            agent: None,
            session: None,
            turn: None,
            pending: None,
            opening: None,
            cancelling: None,
            pump: None,
            prompt: None,
            options: Vec::new(),
            chosen: Vec::new(),
            declines: HashMap::new(),
        }
    }
}

/// Make the user's earlier `chosen` settings the new `session`'s, where the
/// agent offers them; one it refuses is left as the agent has it.
pub(crate) async fn apply_choices(agent: &acp::Agent, session: &str, chosen: &[(String, String)]) {
    for (option, value) in chosen {
        let offered = agent.options(session).into_iter().any(|o| {
            o.id == *option && o.current != *value && o.choices.iter().any(|(v, _)| v == value)
        });
        if offered {
            let _ = agent.set_option(session, option, value).await;
        }
    }
}

/// The MCP and ACP section of the settings modal.
#[derive(Debug, Default)]
pub struct IntegrationsDraft {
    pub mcp: Vec<mcp::ServerConfig>,
    pub acp: Vec<acp::AgentConfig>,
    /// Entries config.toml holds that could not be read.
    pub errors: Vec<String>,
    /// The add-a-server fields: a name; a command line, a URL or a JSON
    /// snippet; and `KEY=value` pairs (environment, or headers for a URL).
    pub new_mcp_name: String,
    pub new_mcp_target: String,
    pub new_mcp_extra: String,
    /// The add-an-agent fields.
    pub new_acp_name: String,
    pub new_acp_command: String,
    /// Each server's connection, as last looked at.
    pub statuses: HashMap<String, mcp::hub::Status>,
    /// The HTTP servers the user is signed in to (MCP's OAuth), by name.
    pub signed_in: HashSet<String>,
    /// What the last action did, or why it did not.
    pub note: Option<String>,
}

/// The `ask_agent` preference: which ACP agent the Ask panel uses.
pub(crate) fn stored_ask_agent() -> Option<String> {
    clew_core::globalconfig::get("ask_agent")
        .and_then(|v| v.as_str().map(str::to_owned))
        .filter(|s| !s.is_empty())
}

/// What to call agent `config` in front of the user: its preset's label when
/// it is one, else its name.
pub(crate) fn acp_label(config: &acp::AgentConfig) -> String {
    acp::config::PRESETS
        .iter()
        .find(|p| p.name == config.name && p.program == config.launch.program)
        .map(|p| p.label.to_string())
        .unwrap_or_else(|| config.name.clone())
}

/// Split a command line as a shell would split words: whitespace separates,
/// single quotes keep everything, double quotes keep all but `\"` and `\\`,
/// and a backslash outside quotes takes the next character as it is.
pub(crate) fn split_command_line(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a ' is not closed".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("a \" is not closed".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("a \" is not closed".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(c) = chars.next() {
                    word.push(c);
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// `KEY=value` (or `Name: value`) pairs, separated by `;` or new lines: each
/// split at whichever of `=` and `:` comes first, so `PATH=/a:/b` and
/// `Authorization: Bearer abc==` both read as meant. What it says of an
/// entry it cannot read does not repeat it (it may be a credential).
pub(crate) fn parse_pairs(text: &str) -> Result<Vec<(String, String)>, String> {
    text.split([';', '\n'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .enumerate()
        .map(|(i, entry)| {
            let at = entry
                .find(['=', ':'])
                .ok_or_else(|| format!("entry {} is not KEY=value (or Name: value)", i + 1))?;
            let (key, value) = (entry[..at].trim(), entry[at + 1..].trim());
            if key.is_empty() {
                return Err(format!("entry {} has no name", i + 1));
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

/// A tool call's arguments as an approval card shows them: each long string
/// shortened on its own (saying by how much), so a padded argument cannot
/// push the one that matters out of view.
fn args_preview(args: &serde_json::Value) -> String {
    let text = match args {
        serde_json::Value::Object(map) if map.is_empty() => "(no arguments)".to_string(),
        args => serde_json::to_string_pretty(&shorten_strings(args)).unwrap_or_default(),
    };
    let total = text.chars().count();
    if total > MAX_DETAIL_CHARS {
        let shown: String = text.chars().take(MAX_DETAIL_CHARS).collect();
        format!(
            "{shown}\n… {} more characters not shown — decline if you cannot tell what this does",
            total - MAX_DETAIL_CHARS
        )
    } else {
        text
    }
}

/// `value` with each string longer than [`MAX_ARG_CHARS`] cut there.
fn shorten_strings(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) if s.chars().count() > MAX_ARG_CHARS => {
            let rest = s.chars().count() - MAX_ARG_CHARS;
            let head: String = s.chars().take(MAX_ARG_CHARS).collect();
            Value::String(format!("{head}… ({rest} more characters)"))
        }
        Value::Array(items) => Value::Array(items.iter().map(shorten_strings).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), shorten_strings(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

impl App {
    /// The enabled MCP servers configured. An entry that could not be read
    /// is named in the status line.
    fn mcp_servers(&mut self) -> Vec<mcp::ServerConfig> {
        let (servers, errors) = mcp::config::load();
        if let Some(error) = errors.first() {
            self.status = format!("{error} — fix it in config.toml");
        }
        servers.into_iter().filter(|s| s.enabled).collect()
    }

    /// Start clew's agent on `question`, with the configured MCP servers'
    /// tools: reached first when there are any, the turn waiting for them
    /// (at most [`GATHER_TIMEOUT`]).
    pub(crate) fn start_agent_ask(&mut self, question: String) -> Task<Message> {
        let servers = self.mcp_servers();
        if servers.is_empty() {
            return self.start_agent_turn(question, Vec::new(), ToolMap::new());
        }
        let token = self.mint_request_id();
        self.proj.inflight.ask_gather = Some(token);
        self.proj.asking = true;
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Ask;
        self.status = format!(
            "Reaching {} MCP server{}…",
            servers.len(),
            if servers.len() == 1 { "" } else { "s" }
        );
        let stamp = self.stamp();
        Task::perform(
            async move {
                mcp::hub::Hub::global()
                    .gather(&servers, GATHER_TIMEOUT)
                    .await
            },
            move |gathered| {
                Message::Ask(AskMsg::McpGathered {
                    stamp: stamp.clone(),
                    token,
                    question: question.clone(),
                    gathered,
                })
            },
        )
    }

    /// The MCP servers answered (or did not in time): start the turn with
    /// the tools of those that did.
    pub(crate) fn on_mcp_gathered(
        &mut self,
        token: u64,
        question: String,
        gathered: Vec<mcp::hub::Gathered>,
    ) -> Task<Message> {
        // Stopped or cleared meanwhile: nothing is asked.
        if self.proj.inflight.ask_gather != Some(token) {
            return Task::none();
        }
        self.proj.inflight.ask_gather = None;
        self.proj.asking = false;
        let mut tools = Vec::new();
        let mut map = ToolMap::new();
        let mut failed = Vec::new();
        for found in gathered {
            match found.outcome {
                Ok((client, list)) => {
                    for tool in list.iter() {
                        let name = mcp::model_tool_name(&found.config.name, &tool.name);
                        if map.contains_key(&name) {
                            continue;
                        }
                        let mut description = format!("[MCP server `{}`] ", found.config.name);
                        if let Some(title) = &tool.title {
                            description.push_str(title);
                            description.push_str(": ");
                        }
                        description.push_str(&tool.description);
                        tools.push(ExternalTool {
                            name: name.clone(),
                            description,
                            parameters: tool.input_schema.clone(),
                        });
                        map.insert(
                            name,
                            ToolTarget {
                                server: found.config.clone(),
                                client: client.clone(),
                                tool: tool.clone(),
                            },
                        );
                    }
                }
                Err(why) => failed.push(format!("{} ({why})", found.config.name)),
            }
        }
        self.status = if failed.is_empty() {
            format!("{} MCP tools available to this answer", tools.len())
        } else {
            format!("Asking without MCP server {}", failed.join(", "))
        };
        if !self.server.is_up() {
            if self.proj.ask_input.trim().is_empty() {
                self.proj.ask_input = question;
            }
            self.status = "clew-server went away — ask again once it is back".into();
            return Task::none();
        }
        self.start_agent_turn(question, tools, map)
    }

    /// The agent turn on `stream` calls MCP tool `tool`: run it, or ask the
    /// user first.
    pub(crate) fn on_agent_tool_called(
        &mut self,
        stream: u64,
        call: u64,
        tool: String,
        args: serde_json::Value,
    ) -> Task<Message> {
        // A call of a turn the user stopped (it crossed the Cancel on its way)
        // is not run, nor asked about.
        let target = self
            .proj
            .link
            .agent_tools
            .get(&stream)
            .filter(|_| self.proj.link.agent_stream == Some(stream))
            .and_then(|tools| tools.get(&tool))
            .cloned();
        let turn = self
            .proj
            .ask_turns
            .iter_mut()
            .find(|t| t.stream == stream && t.streaming);
        let (Some(target), Some(turn)) = (target, turn) else {
            let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                stream,
                call,
                content: format!("`{tool}` is not a tool this turn was given."),
                is_error: true,
            });
            return Task::none();
        };
        let label = format!("{} · {}", target.server.name, target.tool.display_name());
        let ask = target
            .server
            .needs_approval(&target.tool.name, target.tool.read_only);
        turn.extras
            .step_ids
            .insert(format!("mcp:{call}"), turn.steps.len());
        turn.steps.push(AgentStep {
            tool: "mcp".into(),
            title: label.clone(),
            refs: Vec::new(),
            state: if ask {
                StepState::Waiting
            } else {
                StepState::Running
            },
        });
        if ask {
            turn.extras.approvals.push(Approval {
                key: ApprovalKey::Mcp { call },
                title: format!(
                    "The MCP server `{}` wants to run {}",
                    target.server.name,
                    target.tool.display_name()
                ),
                detail: args_preview(&args),
                options: vec![
                    ApprovalOption {
                        id: "once".into(),
                        label: "Allow".into(),
                        allows: true,
                    },
                    ApprovalOption {
                        id: "always".into(),
                        label: format!("Always allow {}", target.tool.name),
                        allows: true,
                    },
                    ApprovalOption {
                        id: "deny".into(),
                        label: "Deny".into(),
                        allows: false,
                    },
                ],
            });
            self.status = format!("{label} waits for your approval in the Ask panel");
            self.proj.link.mcp_pending.insert(
                (stream, call),
                PendingCall {
                    target,
                    args,
                    deadline: call_deadline(),
                },
            );
            return self.ask_to_bottom();
        }
        let run = self.run_mcp_call(stream, call, target, args, call_deadline());
        Task::batch([run, self.ask_to_bottom()])
    }

    /// Run an MCP tool call for the agent turn on `stream`.
    fn run_mcp_call(
        &mut self,
        stream: u64,
        call: u64,
        target: ToolTarget,
        args: serde_json::Value,
        deadline: std::time::Instant,
    ) -> Task<Message> {
        let stamp = self.stamp();
        // No longer than the turn waits for the result.
        let timeout = target
            .server
            .timeout
            .min(deadline.saturating_duration_since(std::time::Instant::now()));
        let (task, handle) = Task::perform(
            async move {
                target
                    .client
                    .call_tool(&target.tool, args, timeout)
                    .await
                    .map_err(|e| e.to_string())
            },
            move |result| {
                Message::Ask(AskMsg::McpToolDone {
                    stamp: stamp.clone(),
                    stream,
                    call,
                    result,
                })
            },
        )
        .abortable();
        self.proj
            .inflight
            .mcp_calls
            .insert((stream, call), handle.abort_on_drop());
        task
    }

    /// An MCP tool call finished: its result goes back to the turn.
    pub(crate) fn on_mcp_tool_done(
        &mut self,
        stream: u64,
        call: u64,
        result: Result<mcp::ToolOutput, String>,
    ) -> Task<Message> {
        // A call that was stopped is owed nothing.
        if self
            .proj
            .inflight
            .mcp_calls
            .remove(&(stream, call))
            .is_none()
        {
            return Task::none();
        }
        let (content, is_error) = match result {
            Ok(out) => (out.text, out.is_error),
            Err(why) => (format!("The MCP server failed: {why}"), true),
        };
        self.set_step_state(
            stream,
            &format!("mcp:{call}"),
            if is_error {
                StepState::Failed
            } else {
                StepState::Done
            },
        );
        let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
            stream,
            call,
            content,
            is_error,
        });
        Task::none()
    }

    fn set_step_state(&mut self, stream: u64, id: &str, state: StepState) {
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream)
            && let Some(&index) = turn.extras.step_ids.get(id)
            && let Some(step) = turn.steps.get_mut(index)
        {
            step.state = state;
        }
    }

    /// The user answered an approval card: an answer that lets the call run
    /// only once the card has been within reach long enough to be read
    /// ([`App::may_allow`]); one the card does not offer, not at all.
    pub(crate) fn on_approve(
        &mut self,
        stream: u64,
        key: ApprovalKey,
        option: String,
    ) -> Task<Message> {
        let allows = self
            .proj
            .ask_turns
            .iter()
            .find(|t| t.stream == stream)
            .and_then(|t| t.extras.approvals.iter().find(|a| a.key == key))
            .and_then(|a| a.options.iter().find(|o| o.id == option))
            .map(|o| o.allows);
        match allows {
            None => return Task::none(),
            Some(true)
                if !self.may_allow(&crate::app::model::Question::Approval { stream, key }) =>
            {
                return Task::none();
            }
            Some(_) => {}
        }
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream) {
            turn.extras.approvals.retain(|a| a.key != key);
        }
        match key {
            ApprovalKey::Mcp { call } => {
                let Some(PendingCall {
                    mut target,
                    args,
                    deadline,
                }) = self.proj.link.mcp_pending.remove(&(stream, call))
                else {
                    return Task::none();
                };
                let id = format!("mcp:{call}");
                if std::time::Instant::now() >= deadline {
                    // The turn went on without it: running it now would act
                    // for nothing.
                    self.set_step_state(stream, &id, StepState::Failed);
                    self.status = "That call waited too long — the agent went on without it".into();
                    return Task::none();
                }
                if option == "deny" {
                    self.set_step_state(stream, &id, StepState::Failed);
                    let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                        stream,
                        call,
                        content: "The user declined to run this tool. Do not ask for it again \
                                  in this answer; go on without it."
                            .into(),
                        is_error: true,
                    });
                    return Task::none();
                }
                if option == "always" {
                    let (server, tool) = (target.server.name.clone(), target.tool.name.clone());
                    match mcp::config::allow_tool(&server, &tool) {
                        Ok(()) => {
                            self.status = format!("{tool} of `{server}` will no longer ask");
                        }
                        Err(e) => self.status = format!("Could not remember the approval: {e}"),
                    }
                    // This turn's later calls of the tool need not ask either.
                    if let Some(tools) = self.proj.link.agent_tools.get_mut(&stream) {
                        for t in Arc::make_mut(tools).values_mut() {
                            if t.server.name == server && t.tool.name == tool {
                                t.server.allow.push(tool.clone());
                            }
                        }
                    }
                    target.server.allow.push(tool);
                }
                self.set_step_state(stream, &id, StepState::Running);
                self.run_mcp_call(stream, call, target, args, deadline)
            }
            ApprovalKey::Acp { id } => {
                // The decline clew added where the agent offered none is the
                // request cancelled (`with_a_decline`): told by the id it
                // was given, which no answer of the agent's has.
                let added = self.proj.acp.as_mut().and_then(|l| l.declines.remove(&id));
                let cancelled = allows == Some(false) && added.as_deref() == Some(option.as_str());
                // Only the running turn's agent is answered: a card of a turn
                // that ended (or of an agent since replaced) answers nothing.
                if let Some(agent) = self
                    .proj
                    .acp
                    .as_ref()
                    .filter(|l| l.turn == Some(stream))
                    .and_then(|l| l.agent.as_ref())
                {
                    agent.answer_permission(id, (!cancelled).then_some(option.as_str()));
                }
                Task::none()
            }
        }
    }

    /// Forget a finished agent turn's MCP state: its tools, its calls still
    /// waiting for approval (and their cards), its calls running.
    pub(crate) fn drop_turn_tools(&mut self, stream: u64) {
        self.proj.link.agent_tools.remove(&stream);
        self.proj.link.mcp_pending.retain(|(s, _), _| *s != stream);
        self.proj
            .inflight
            .mcp_calls
            .retain(|(s, _), _| *s != stream);
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream) {
            turn.extras.approvals.clear();
            for step in &mut turn.steps {
                if matches!(step.state, StepState::Running | StepState::Waiting) {
                    step.state = StepState::Failed;
                }
            }
        }
    }

    /// Stop the MCP and ACP parts of the answer in progress: the tool calls
    /// waiting or running, the question still gathering its tools, the ACP
    /// agent's turn (cancelled on the agent, which answers its permission
    /// requests). Returns the streams of the ACP turns stopped — the one
    /// running, and a question that had not reached the agent yet — which the
    /// caller closes as stopped.
    pub(crate) fn stop_integrations(&mut self) -> Vec<u64> {
        self.proj.inflight.ask_gather = None;
        let streams: Vec<u64> = self
            .proj
            .link
            .mcp_pending
            .keys()
            .chain(self.proj.inflight.mcp_calls.keys())
            .map(|(s, _)| *s)
            .collect();
        for stream in streams {
            self.drop_turn_tools(stream);
        }
        self.stop_acp()
    }

    /// Stop the ACP agent's turn (it is asked to cancel it) and drop the
    /// question waiting for the agent; the streams whose turns the caller
    /// closes.
    fn stop_acp(&mut self) -> Vec<u64> {
        let Some(link) = self.proj.acp.as_mut() else {
            return Vec::new();
        };
        let mut stopped = Vec::new();
        if let Some(stream) = link.turn.take() {
            if let (Some(agent), Some(session)) = (&link.agent, &link.session) {
                agent.cancel(session);
            }
            link.cancelling = Some(stream);
            stopped.push(stream);
        }
        stopped.extend(link.pending.take().map(|(stream, _)| stream));
        for stream in &stopped {
            if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == *stream) {
                turn.extras.approvals.clear();
                turn.extras.thought.clear();
            }
        }
        stopped
    }

    // -- ACP ------------------------------------------------------------------

    /// End the project's ACP agent: its turn, or the question waiting for
    /// it, closed as Stop closes them, then the agent itself.
    pub(crate) fn end_acp_agent(&mut self) -> Task<Message> {
        let stopped = self.stop_acp();
        self.proj.acp = None;
        let mut closed = Task::none();
        for stream in stopped {
            let ended = self.on_ask_stream_ended(stream, clew_protocol::StreamOutcome::Stopped);
            closed = Task::batch([closed, ended]);
        }
        closed
    }

    /// Ask the ACP agent `name` the question.
    pub(crate) fn start_acp_ask(&mut self, name: String, question: String) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            self.status = "Open a project to ask an agent about it".into();
            return Task::none();
        };
        if self.connection.is_remote() {
            self.status = "ACP agents run on this machine — they answer about local projects. \
                           Pick clew's own agent for this one."
                .into();
            return Task::none();
        }
        let Some(config) = self.acp_agents.iter().find(|a| a.name == name).cloned() else {
            self.status = format!("No ACP agent `{name}` is configured any more — pick another");
            self.ask_agent = None;
            return Task::none();
        };
        let label = acp_label(&config);
        // Client-side grounding the agent cannot see, as for clew's agent.
        let mut context = String::new();
        if let Some(state) = self.debug_context() {
            context.push_str(&state);
        }
        for pin in &self.proj.ask_pins {
            context.push_str(&crate::app::ask::pin_context(pin));
        }
        let text = if context.trim().is_empty() {
            question.clone()
        } else {
            format!("{question}\n\n{context}")
        };
        let blocks = vec![acp::text_block(&text)];
        // One agent per project: asking another ends the one running.
        let closing = if self.proj.acp.as_ref().is_some_and(|l| l.name != name) {
            self.end_acp_agent()
        } else {
            Task::none()
        };
        let stream = self.mint_request_id();
        self.proj.ask_input.clear();
        self.proj.ask_turns.push(AskTurn {
            stream,
            question,
            streaming: true,
            extras: TurnExtras {
                agent: Some(label.clone()),
                ..TurnExtras::default()
            },
            ..AskTurn::default()
        });
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Ask;
        let link = self
            .proj
            .acp
            .get_or_insert_with(|| AcpLink::new(name.clone(), label.clone()));
        link.pending = Some((stream, blocks));
        let to_bottom = self.ask_to_bottom();
        let next = match (
            self.proj.acp.as_ref().and_then(|l| l.agent.clone()),
            self.proj.acp.as_ref().and_then(|l| l.session.clone()),
        ) {
            (Some(_), Some(_)) => self.send_acp_prompt(),
            (Some(agent), None) => self.open_acp_session(agent, &config, &root),
            (None, _) => {
                if self.proj.acp.as_ref().is_some_and(|l| l.pump.is_some()) {
                    // Starting already: the question goes once it is up.
                    Task::none()
                } else {
                    self.status = format!("Starting {label}…");
                    self.start_acp_agent(config, root)
                }
            }
        };
        Task::batch([closing, next, to_bottom])
    }

    /// The MCP servers handed to an ACP agent: clew's own code tools, and
    /// the user's enabled servers when the agent is set to share them.
    fn acp_mcp_servers(
        &mut self,
        config: &acp::AgentConfig,
        root: &Path,
    ) -> Vec<acp::McpServerSpec> {
        let mut servers = Vec::new();
        if let Some(command) = crate::backend::server::server_bin_path() {
            servers.push(acp::McpServerSpec::Stdio {
                name: "clew".into(),
                command,
                args: vec![
                    "mcp".into(),
                    "--root".into(),
                    root.to_string_lossy().into_owned(),
                ],
                env: Vec::new(),
            });
        }
        if config.share_mcp {
            for server in self.mcp_servers() {
                match server.transport {
                    mcp::Transport::Stdio(launch) => {
                        let Some(program) = clew_core::linerpc::resolve_program(&launch.program)
                        else {
                            continue;
                        };
                        // The agent starts it wherever the agent is (the
                        // project, likely): it starts in a neutral folder, or
                        // its own, as when clew starts it.
                        let dir = launch
                            .cwd
                            .clone()
                            .unwrap_or_else(clew_core::linerpc::neutral_dir);
                        let (command, args) =
                            clew_core::linerpc::launched_in(program, launch.args, &dir);
                        servers.push(acp::McpServerSpec::Stdio {
                            name: server.name,
                            command,
                            args,
                            env: launch.env,
                        });
                    }
                    mcp::Transport::Http { url, mut headers } => {
                        // The agent reaches the server itself: it gets the
                        // user's sign-in, as it gets their headers.
                        let has_auth = headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                        if !has_auth
                            && let Some(token) = mcp::oauth::session(&url).map(|s| s.access_token)
                        {
                            headers.push(("Authorization".into(), format!("Bearer {token}")));
                        }
                        servers.push(acp::McpServerSpec::Http {
                            name: server.name,
                            url,
                            headers,
                        });
                    }
                }
            }
        }
        servers
    }

    /// Start agent `config` and its first session, then forward its events
    /// for as long as it runs.
    fn start_acp_agent(&mut self, config: acp::AgentConfig, root: PathBuf) -> Task<Message> {
        let mcp = self.acp_mcp_servers(&config, &root);
        let stamp = self.stamp();
        let name = config.name.clone();
        let chosen = self
            .proj
            .acp
            .as_ref()
            .map(|l| l.chosen.clone())
            .unwrap_or_default();
        let stream = iced::stream::channel(
            256,
            move |mut out: iced::futures::channel::mpsc::Sender<Message>| async move {
                let started = async {
                    let (agent, events) = acp::Agent::start(&config, &root)
                        .await
                        .map_err(|e| e.to_string())?;
                    let agent = Arc::new(agent);
                    let session = agent.new_session(&mcp).await.map_err(|e| e.to_string())?;
                    apply_choices(&agent, &session, &chosen).await;
                    Ok::<_, String>((agent, session, events))
                }
                .await;
                let (agent, session, mut events) = match started {
                    Ok(started) => started,
                    Err(e) => {
                        let _ = out
                            .send(Message::Ask(AskMsg::AcpReady {
                                stamp,
                                name,
                                result: Err(e),
                            }))
                            .await;
                        return;
                    }
                };
                let ready = Message::Ask(AskMsg::AcpReady {
                    stamp: stamp.clone(),
                    name,
                    result: Ok((agent.clone(), session)),
                });
                if out.send(ready).await.is_err() {
                    return;
                }
                // The agent is the link's now: this task holds it only for
                // as long as it forwards.
                drop(agent);
                while let Some(event) = events.recv().await {
                    let msg = Message::Ask(AskMsg::AcpEvent {
                        stamp: stamp.clone(),
                        event,
                    });
                    if out.send(msg).await.is_err() {
                        return;
                    }
                }
            },
        );
        let (task, handle) = Task::run(stream, |m| m).abortable();
        if let Some(link) = self.proj.acp.as_mut() {
            link.pump = Some(handle.abort_on_drop());
        }
        task
    }

    /// Open a new session with the running agent (after Ask Clear).
    fn open_acp_session(
        &mut self,
        agent: Arc<acp::Agent>,
        config: &acp::AgentConfig,
        root: &Path,
    ) -> Task<Message> {
        if self.proj.acp.as_ref().is_some_and(|l| l.opening.is_some()) {
            return Task::none();
        }
        let mcp = self.acp_mcp_servers(config, root);
        let stamp = self.stamp();
        let chosen = self
            .proj
            .acp
            .as_ref()
            .map(|l| l.chosen.clone())
            .unwrap_or_default();
        let (task, handle) = Task::perform(
            async move {
                let session = agent.new_session(&mcp).await.map_err(|e| e.to_string())?;
                apply_choices(&agent, &session, &chosen).await;
                Ok(session)
            },
            move |result| {
                Message::Ask(AskMsg::AcpSession {
                    stamp: stamp.clone(),
                    result,
                })
            },
        )
        .abortable();
        if let Some(link) = self.proj.acp.as_mut() {
            link.opening = Some(handle.abort_on_drop());
        }
        task
    }

    /// The agent started (or did not).
    pub(crate) fn on_acp_ready(
        &mut self,
        name: String,
        result: Result<(Arc<acp::Agent>, String), String>,
    ) -> Task<Message> {
        let Some(link) = self.proj.acp.as_mut().filter(|l| l.name == name) else {
            return Task::none();
        };
        match result {
            Ok((agent, session)) => {
                self.status = format!("{} is ready", agent.info().display_name());
                link.label = agent.info().display_name().to_string();
                link.options = agent.options(&session);
                link.agent = Some(agent);
                link.session = Some(session);
                self.send_acp_prompt()
            }
            Err(why) => {
                let pending = link.pending.take();
                let label = link.label.clone();
                self.proj.acp = None;
                self.status = format!("{label} could not start: {why}");
                match pending {
                    Some((stream, _)) => self.on_ask_stream_ended(
                        stream,
                        clew_protocol::StreamOutcome::Failed(format!(
                            "{label} could not start: {why}"
                        )),
                    ),
                    None => Task::none(),
                }
            }
        }
    }

    /// A new session opened (or did not).
    pub(crate) fn on_acp_session(&mut self, result: Result<String, String>) -> Task<Message> {
        let Some(link) = self.proj.acp.as_mut() else {
            return Task::none();
        };
        link.opening = None;
        match result {
            Ok(session) => {
                if let Some(agent) = &link.agent {
                    link.options = agent.options(&session);
                }
                link.session = Some(session);
                self.send_acp_prompt()
            }
            Err(why) => {
                let pending = link.pending.take();
                match pending {
                    Some((stream, _)) => self.on_ask_stream_ended(
                        stream,
                        clew_protocol::StreamOutcome::Failed(format!(
                            "the agent could not open a conversation: {why}"
                        )),
                    ),
                    None => Task::none(),
                }
            }
        }
    }

    /// Send the question waiting for the agent.
    fn send_acp_prompt(&mut self) -> Task<Message> {
        let stamp = self.stamp();
        let Some(link) = self.proj.acp.as_mut() else {
            return Task::none();
        };
        let (Some(agent), Some(session)) = (link.agent.clone(), link.session.clone()) else {
            return Task::none();
        };
        // One turn at a time: the next waits for this one's end, and for a
        // stopped one's — for a while: an agent that ignores the cancel is
        // started afresh rather than holding the question for the rest of
        // its turn.
        if link.turn.is_some() {
            return Task::none();
        }
        if let Some(stopped) = link.cancelling {
            if link.pending.is_none() {
                return Task::none();
            }
            return Task::perform(
                async { tokio::time::sleep(CANCEL_GRACE).await },
                move |()| {
                    Message::Ask(AskMsg::AcpCancelOverdue {
                        stamp: stamp.clone(),
                        stream: stopped,
                    })
                },
            );
        }
        let Some((stream, blocks)) = link.pending.take() else {
            return Task::none();
        };
        link.turn = Some(stream);
        let (task, handle) = Task::perform(
            async move {
                agent
                    .prompt(&session, blocks)
                    .await
                    .map_err(|e| e.to_string())
            },
            move |result| {
                Message::Ask(AskMsg::AcpTurnEnded {
                    stamp: stamp.clone(),
                    stream,
                    result,
                })
            },
        )
        .abortable();
        link.prompt = Some(handle.abort_on_drop());
        task
    }

    /// The agent has not ended the turn stopped on `stream` though asked to,
    /// and a question waits: start the agent afresh for it.
    pub(crate) fn on_acp_cancel_overdue(&mut self, stream: u64) -> Task<Message> {
        let Some(link) = self.proj.acp.as_mut() else {
            return Task::none();
        };
        if link.cancelling != Some(stream) || link.pending.is_none() {
            return Task::none();
        }
        let (name, label, pending) = (link.name.clone(), link.label.clone(), link.pending.take());
        let chosen = std::mem::take(&mut link.chosen);
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        let config = self.acp_agents.iter().find(|a| a.name == name).cloned();
        // Dropping the link ends the agent that would not stop.
        self.proj.acp = None;
        let (Some(root), Some(config)) = (root, config) else {
            return match pending {
                Some((stream, _)) => self.on_ask_stream_ended(
                    stream,
                    clew_protocol::StreamOutcome::Failed(format!(
                        "{label} did not stop its last answer"
                    )),
                ),
                None => Task::none(),
            };
        };
        let mut link = AcpLink::new(name, label.clone());
        link.pending = pending;
        link.chosen = chosen;
        self.proj.acp = Some(link);
        self.status = format!("{label} did not stop its last answer — starting it again");
        self.start_acp_agent(config, root)
    }

    /// What the agent reported.
    pub(crate) fn on_acp_event(&mut self, event: acp::AgentEvent) -> Task<Message> {
        let Some(link) = self.proj.acp.as_ref() else {
            return Task::none();
        };
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        let rel_of = |path: &Path| -> Option<String> {
            let root = root.as_ref()?;
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            path.strip_prefix(&root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
        };
        match event {
            acp::AgentEvent::Exited(why) => {
                let label = link.label.clone();
                let turn = link.turn.or(link.pending.as_ref().map(|(s, _)| *s));
                let tail = link
                    .agent
                    .as_ref()
                    .map(|a| a.stderr_tail())
                    .unwrap_or_default();
                self.proj.acp = None;
                self.status = format!("{label} exited ({why})");
                let detail = if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n\n```\n{tail}\n```")
                };
                return match turn {
                    Some(stream) => self.on_ask_stream_ended(
                        stream,
                        clew_protocol::StreamOutcome::Failed(format!(
                            "{label} exited ({why}){detail}"
                        )),
                    ),
                    None => Task::none(),
                };
            }
            acp::AgentEvent::Options { session, options } => {
                if let Some(link) = self.proj.acp.as_mut()
                    && link.session.as_deref() == Some(session.as_str())
                {
                    link.options = options;
                }
                return Task::none();
            }
            acp::AgentEvent::FileWritten { path } => {
                self.status = format!(
                    "{} wrote {}",
                    link.label,
                    rel_of(&path).unwrap_or_else(|| path.display().to_string())
                );
                return Task::none();
            }
            _ => {}
        }
        let Some(stream) = link.turn else {
            // Between turns nothing is shown; a permission asked then is
            // refused, as nobody is there to answer it.
            if let (acp::AgentEvent::Permission(request), Some(agent)) = (&event, &link.agent) {
                agent.answer_permission(request.id, None);
            }
            return Task::none();
        };
        let label = link.label.clone();
        let agent = link.agent.clone();
        if let acp::AgentEvent::Text { text, .. } = &event {
            return self.on_ask_delta(stream, text.clone());
        }
        let Some(turn) = self
            .proj
            .ask_turns
            .iter_mut()
            .find(|t| t.stream == stream && t.streaming)
        else {
            // The turn is not on screen any more: a permission asked for it
            // is refused, as between turns — dropped, the agent waited on it
            // for good.
            if let (acp::AgentEvent::Permission(request), Some(agent)) = (&event, &agent) {
                agent.answer_permission(request.id, None);
            }
            return Task::none();
        };
        let refs_of = |locations: &Option<Vec<(PathBuf, Option<u32>)>>| {
            locations
                .iter()
                .flatten()
                .filter_map(|(path, line)| Some((rel_of(path)?, line.map(|l| l as usize))))
                .collect::<Vec<_>>()
        };
        let state_of = |status: Option<acp::ToolStatus>| match status {
            Some(acp::ToolStatus::Completed) => StepState::Done,
            Some(acp::ToolStatus::Failed) => StepState::Failed,
            _ => StepState::Running,
        };
        match event {
            acp::AgentEvent::Thought { text, .. } => {
                turn.extras.thought.push_str(&text);
                let len = turn.extras.thought.chars().count();
                if len > MAX_THOUGHT_CHARS {
                    turn.extras.thought = turn
                        .extras
                        .thought
                        .chars()
                        .skip(len - MAX_THOUGHT_CHARS)
                        .collect();
                }
                Task::none()
            }
            acp::AgentEvent::ToolCall { call, .. } => {
                let mut title = call.title.clone().unwrap_or_else(|| "a tool".into());
                if let Some(summary) = &call.summary {
                    title = format!("{title} — {summary}");
                }
                turn.extras
                    .step_ids
                    .insert(call.id.clone(), turn.steps.len());
                turn.steps.push(AgentStep {
                    tool: call.kind.clone().unwrap_or_else(|| "other".into()),
                    title,
                    refs: refs_of(&call.locations),
                    state: state_of(call.status),
                });
                self.ask_to_bottom()
            }
            acp::AgentEvent::ToolCallUpdate { call, .. } => {
                match turn.extras.step_ids.get(&call.id).copied() {
                    Some(index) if index < turn.steps.len() => {
                        let step = &mut turn.steps[index];
                        if let Some(title) = &call.title {
                            step.title = title.clone();
                        }
                        if let Some(summary) = &call.summary {
                            let base = step
                                .title
                                .split(" — ")
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            step.title = format!("{base} — {summary}");
                        }
                        if let Some(kind) = &call.kind {
                            step.tool = kind.clone();
                        }
                        if call.status.is_some() {
                            step.state = state_of(call.status);
                        }
                        if call.locations.is_some() {
                            step.refs = refs_of(&call.locations);
                        }
                    }
                    _ => {
                        turn.extras
                            .step_ids
                            .insert(call.id.clone(), turn.steps.len());
                        turn.steps.push(AgentStep {
                            tool: call.kind.clone().unwrap_or_else(|| "other".into()),
                            title: call.title.clone().unwrap_or_else(|| "a tool".into()),
                            refs: refs_of(&call.locations),
                            state: state_of(call.status),
                        });
                    }
                }
                Task::none()
            }
            acp::AgentEvent::Plan { entries, .. } => {
                turn.extras.plan = entries;
                Task::none()
            }
            acp::AgentEvent::Permission(request) => {
                if let Some(&index) = turn.extras.step_ids.get(&request.tool.id)
                    && let Some(step) = turn.steps.get_mut(index)
                {
                    step.state = StepState::Waiting;
                }
                let what = request
                    .tool
                    .title
                    .clone()
                    .unwrap_or_else(|| "use a tool".into());
                let mut detail = String::new();
                if let Some(kind) = &request.tool.kind {
                    detail.push_str(&format!("kind: {kind}"));
                }
                for (path, line) in request.tool.locations.iter().flatten() {
                    detail.push_str(&format!(
                        "\n{}{}",
                        rel_of(path).unwrap_or_else(|| path.display().to_string()),
                        line.map(|l| format!(":{l}")).unwrap_or_default()
                    ));
                }
                let (options, added) = with_a_decline(
                    request
                        .options
                        .iter()
                        .map(|o| ApprovalOption {
                            id: o.id.clone(),
                            label: o.name.clone(),
                            allows: o.kind.starts_with("allow"),
                        })
                        .collect(),
                );
                turn.extras.approvals.push(Approval {
                    key: ApprovalKey::Acp { id: request.id },
                    title: format!("{label} asks to {what}"),
                    detail: detail.trim().to_string(),
                    options,
                });
                if let (Some(added), Some(link)) = (added, self.proj.acp.as_mut()) {
                    link.declines.insert(request.id, added);
                }
                self.status = format!("{label} waits for your answer in the Ask panel");
                self.ask_to_bottom()
            }
            _ => Task::none(),
        }
    }

    /// The agent ended the turn on `stream`.
    pub(crate) fn on_acp_turn_ended(
        &mut self,
        stream: u64,
        result: Result<acp::StopReason, String>,
    ) -> Task<Message> {
        use clew_protocol::StreamOutcome;
        let mut next = Task::none();
        if let Some(link) = self.proj.acp.as_mut() {
            if link.turn == Some(stream) {
                link.turn = None;
                link.prompt = None;
            }
            if link.cancelling == Some(stream) {
                link.cancelling = None;
                link.prompt = None;
            }
        }
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream) {
            turn.extras.approvals.clear();
            turn.extras.thought.clear();
            for step in &mut turn.steps {
                if matches!(step.state, StepState::Running | StepState::Waiting) {
                    step.state = StepState::Failed;
                }
            }
            let note = match &result {
                Ok(acp::StopReason::MaxTokens) => Some("*(the agent ran out of tokens)*"),
                Ok(acp::StopReason::MaxTurnRequests) => {
                    Some("*(the agent reached its step limit)*")
                }
                _ => None,
            };
            if let Some(note) = note {
                turn.answer_md.push_str("\n\n");
                turn.answer_md.push_str(note);
            }
        }
        let outcome = match result {
            Ok(acp::StopReason::Cancelled) => StreamOutcome::Stopped,
            Ok(acp::StopReason::Refusal) => {
                StreamOutcome::Failed("the agent declined to answer".into())
            }
            Ok(_) => StreamOutcome::Done,
            Err(why) => StreamOutcome::Failed(why),
        };
        let ended = self.on_ask_stream_ended(stream, outcome);
        // A question asked while this one ran goes now.
        if self.proj.acp.as_ref().is_some_and(|l| l.pending.is_some()) {
            next = self.send_acp_prompt();
        }
        Task::batch([ended, next])
    }

    /// Choose `value` for the ACP session's setting `option`.
    pub(crate) fn on_acp_option(&mut self, option: String, value: String) -> Task<Message> {
        let Some(link) = self.proj.acp.as_ref() else {
            return Task::none();
        };
        let (Some(agent), Some(session)) = (link.agent.clone(), link.session.clone()) else {
            return Task::none();
        };
        let stamp = self.stamp();
        Task::perform(
            async move {
                let result = agent
                    .set_option(&session, &option, &value)
                    .await
                    .map_err(|e| e.to_string());
                (agent.id(), session, option, value, result)
            },
            move |(agent, session, option, value, result)| {
                Message::Ask(AskMsg::AcpOptionSet {
                    stamp: stamp.clone(),
                    agent,
                    session,
                    option,
                    value,
                    result,
                })
            },
        )
    }

    /// A choice of a session setting took effect (or did not) — applied to
    /// the session it was made in only: one since replaced (Clear, another
    /// agent, a restart) has settings of its own.
    pub(crate) fn on_acp_option_set(
        &mut self,
        agent: u64,
        session: String,
        option: String,
        value: String,
        result: Result<Vec<acp::ConfigOption>, String>,
    ) -> Task<Message> {
        let Some(link) = self.proj.acp.as_mut().filter(|l| {
            l.session.as_deref() == Some(session.as_str())
                && l.agent.as_ref().is_some_and(|a| a.id() == agent)
        }) else {
            return Task::none();
        };
        match result {
            Ok(options) => {
                link.options = options;
                link.chosen.retain(|(o, _)| *o != option);
                link.chosen.push((option, value));
            }
            Err(why) => self.status = format!("The agent did not take the setting: {why}"),
        }
        Task::none()
    }

    /// Pick who answers the Ask panel's questions.
    pub(crate) fn on_pick_ask_agent(&mut self, agent: Option<String>) -> Task<Message> {
        if self.ask_agent == agent {
            return Task::none();
        }
        self.ask_agent = agent.clone();
        let value = toml::Value::String(agent.clone().unwrap_or_default());
        if let Err(e) = clew_core::globalconfig::set_keys(vec![("ask_agent".into(), value)]) {
            self.status = format!("Could not remember the choice: {e}");
        } else {
            self.status = match agent
                .as_deref()
                .and_then(|n| self.acp_agents.iter().find(|a| a.name == n))
            {
                Some(config) => format!("Questions go to {}", acp_label(config)),
                None => "Questions go to clew's own agent".into(),
            };
        }
        Task::none()
    }

    /// Keep the Ask conversation pinned to the bottom.
    pub(crate) fn ask_to_bottom(&self) -> Task<Message> {
        operation::scroll_to(
            ui::ask_scroll_id(),
            AbsoluteOffset {
                x: 0.0,
                y: f32::MAX,
            },
        )
    }

    // -- Settings ---------------------------------------------------------------

    /// Read the MCP and ACP entries into the settings form.
    pub(crate) fn refresh_integrations(&mut self) {
        let (mcp, mut errors) = mcp::config::load();
        let (acp, acp_errors) = acp::config::load();
        errors.extend(acp_errors);
        let hub = mcp::hub::Hub::global();
        let names: Vec<String> = mcp.iter().map(|s| s.name.clone()).collect();
        hub.retain(&names);
        let draft = &mut self.settings.integrations;
        draft.statuses = names.iter().map(|n| (n.clone(), hub.status(n))).collect();
        draft.signed_in = mcp
            .iter()
            .filter(|s| matches!(&s.transport, mcp::Transport::Http { url, .. } if mcp::oauth::signed_in(url)))
            .map(|s| s.name.clone())
            .collect();
        draft.mcp = mcp;
        draft.acp = acp.clone();
        draft.errors = errors;
        self.acp_agents = acp;
        if let Some(agent) = &self.ask_agent
            && !self.acp_agents.iter().any(|a| &a.name == agent)
        {
            self.ask_agent = None;
        }
    }

    /// Handle an [`IntegrationsMsg`].
    pub(crate) fn update_integrations(&mut self, message: IntegrationsMsg) -> Task<Message> {
        let mut closing = Task::none();
        let draft = &mut self.settings.integrations;
        let result: Result<Option<String>, String> = match message {
            IntegrationsMsg::McpName(v) => {
                draft.new_mcp_name = v;
                return Task::none();
            }
            IntegrationsMsg::McpTarget(v) => {
                draft.new_mcp_target = v;
                return Task::none();
            }
            IntegrationsMsg::McpExtra(v) => {
                draft.new_mcp_extra = v;
                return Task::none();
            }
            IntegrationsMsg::AcpName(v) => {
                draft.new_acp_name = v;
                return Task::none();
            }
            IntegrationsMsg::AcpCommand(v) => {
                draft.new_acp_command = v;
                return Task::none();
            }
            IntegrationsMsg::McpAdd => self.add_mcp_from_form(),
            IntegrationsMsg::McpRemove(name) => {
                mcp::hub::Hub::global().forget(&name);
                mcp::config::remove(&name).map(|()| Some(format!("Removed MCP server `{name}`")))
            }
            IntegrationsMsg::McpEnable(name, on) => {
                mcp::hub::Hub::global().forget(&name);
                mcp::config::set_enabled(&name, on).map(|()| None)
            }
            IntegrationsMsg::McpApprove(name, approve) => {
                mcp::config::set_approve(&name, approve).map(|()| None)
            }
            IntegrationsMsg::McpTest(name) => {
                let Some(config) = draft.mcp.iter().find(|s| s.name == name).cloned() else {
                    return Task::none();
                };
                mcp::hub::Hub::global().forget(&name);
                draft
                    .statuses
                    .insert(name.clone(), mcp::hub::Status::Connecting);
                draft.note = Some(format!("Connecting to `{name}`…"));
                let mut config = config;
                config.enabled = true;
                return Task::perform(
                    async move {
                        let found = mcp::hub::Hub::global()
                            .gather(std::slice::from_ref(&config), GATHER_TIMEOUT)
                            .await;
                        let outcome = found
                            .into_iter()
                            .next()
                            .map(|g| g.outcome.map(|(c, tools)| (c.info().clone(), tools.len())))
                            .unwrap_or_else(|| Err("nothing to connect to".into()));
                        (config.name, outcome)
                    },
                    |(name, outcome)| {
                        Message::Integrations(IntegrationsMsg::McpTested { name, outcome })
                    },
                );
            }
            IntegrationsMsg::McpTested { name, outcome } => Ok(Some(match outcome {
                Ok((info, tools)) => format!(
                    "`{name}` works: {} {} (MCP {}), {tools} tools",
                    if info.name.is_empty() {
                        "the server"
                    } else {
                        &info.name
                    },
                    info.version,
                    info.era.version()
                ),
                Err(why) => format!("`{name}` failed: {why}"),
            })),
            IntegrationsMsg::McpSignIn(name) => {
                let Some(url) =
                    draft
                        .mcp
                        .iter()
                        .find(|s| s.name == name)
                        .and_then(|s| match &s.transport {
                            mcp::Transport::Http { url, .. } => Some(url.clone()),
                            mcp::Transport::Stdio(_) => None,
                        })
                else {
                    return Task::none();
                };
                draft.note = Some(format!(
                    "Signing in to `{name}` — finish in your browser (within {} minutes)…",
                    mcp::oauth::SIGN_IN_TIMEOUT.as_secs() / 60
                ));
                let client = mcp::config::oauth_client(&name);
                return Task::perform(
                    async move {
                        let result = tokio::task::spawn_blocking(move || {
                            mcp::oauth::sign_in(&url, client, |page| {
                                crate::app::content::open_in_browser(page)
                            })
                        })
                        .await
                        .unwrap_or_else(|_| Err("the sign-in failed unexpectedly".into()));
                        (name, result)
                    },
                    |(name, result)| {
                        Message::Integrations(IntegrationsMsg::McpSignedIn { name, result })
                    },
                );
            }
            IntegrationsMsg::McpSignedIn { name, result } => match result {
                Ok(()) => {
                    mcp::hub::Hub::global().forget(&name);
                    Ok(Some(format!(
                        "Signed in to `{name}` — Test it, or just ask"
                    )))
                }
                Err(why) => Ok(Some(format!("Signing in to `{name}` failed: {why}"))),
            },
            IntegrationsMsg::McpSignOut(name) => {
                match draft
                    .mcp
                    .iter()
                    .find(|s| s.name == name)
                    .map(|s| &s.transport)
                {
                    Some(mcp::Transport::Http { url, .. }) => {
                        mcp::hub::Hub::global().forget(&name);
                        // Off the UI thread, as signing in is: the store's
                        // locks are held across a renewal's token request,
                        // which may take its whole timeout, and this thread
                        // serves every window.
                        let url = url.clone();
                        draft.note = Some(format!("Signing out of `{name}`…"));
                        return Task::perform(
                            async move {
                                let result =
                                    tokio::task::spawn_blocking(move || mcp::oauth::sign_out(&url))
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err("the sign-out failed unexpectedly".into())
                                        });
                                (name, result)
                            },
                            |(name, result)| {
                                Message::Integrations(IntegrationsMsg::McpSignedOut {
                                    name,
                                    result,
                                })
                            },
                        );
                    }
                    _ => Ok(None),
                }
            }
            IntegrationsMsg::McpSignedOut { name, result } => {
                // A connection made while the store still held the sign-in
                // carries it: dropped again now that it does not.
                mcp::hub::Hub::global().forget(&name);
                result.map(|()| Some(format!("Signed out of `{name}`")))
            }
            IntegrationsMsg::AcpAdd => self.add_acp_from_form(),
            IntegrationsMsg::AcpPreset(index) => match acp::config::PRESETS.get(index) {
                Some(preset) => acp::config::save(&preset.config())
                    .map(|()| Some(format!("Added {} — pick it in the Ask panel", preset.label))),
                None => Ok(None),
            },
            IntegrationsMsg::AcpRemove(name) => {
                if self.proj.acp.as_ref().is_some_and(|l| l.name == name) {
                    closing = self.end_acp_agent();
                }
                acp::config::remove(&name).map(|()| Some(format!("Removed agent `{name}`")))
            }
            IntegrationsMsg::AcpShareMcp(name, share) => {
                match self
                    .settings
                    .integrations
                    .acp
                    .iter()
                    .find(|a| a.name == name)
                    .cloned()
                {
                    Some(mut agent) => {
                        agent.share_mcp = share;
                        // A running agent got its servers at its session's start.
                        if self.proj.acp.as_ref().is_some_and(|l| l.name == name) {
                            closing = self.end_acp_agent();
                        }
                        acp::config::save(&agent).map(|()| None)
                    }
                    None => Ok(None),
                }
            }
        };
        match result {
            Ok(note) => {
                if note.is_some() {
                    self.settings.integrations.note = note;
                }
            }
            Err(e) => self.settings.integrations.note = Some(format!("Not saved: {e}")),
        }
        self.refresh_integrations();
        closing
    }

    /// Add the MCP server the form describes: a command line, a URL, or a
    /// JSON snippet of one or more servers.
    fn add_mcp_from_form(&mut self) -> Result<Option<String>, String> {
        let draft = &mut self.settings.integrations;
        let target = draft.new_mcp_target.trim().to_string();
        if target.is_empty() {
            return Err("give a command, a URL, or a JSON snippet".into());
        }
        let servers = if target.starts_with('{') {
            mcp::config::from_json(&target)?
        } else {
            let name = draft.new_mcp_name.trim();
            if !mcp::config::valid_name(name) {
                return Err(format!(
                    "give the server a name: letters, digits, - and _ (at most {})",
                    mcp::config::MAX_NAME_LEN
                ));
            }
            let extra = parse_pairs(&draft.new_mcp_extra)?;
            let transport = if target.starts_with("http://") || target.starts_with("https://") {
                mcp::config::check_headers(&extra)?;
                mcp::Transport::Http {
                    url: target.clone(),
                    headers: extra,
                }
            } else {
                let mut words = split_command_line(&target)?;
                if words.is_empty() {
                    return Err("an empty command".into());
                }
                let program = words.remove(0);
                mcp::Transport::Stdio(clew_core::linerpc::Launch {
                    program,
                    args: words,
                    env: extra,
                    cwd: None,
                })
            };
            vec![mcp::ServerConfig {
                name: name.to_string(),
                transport,
                enabled: true,
                approve: mcp::Approve::Ask,
                allow: Vec::new(),
                timeout: mcp::config::DEFAULT_TIMEOUT,
            }]
        };
        for server in &servers {
            if let mcp::Transport::Stdio(launch) = &server.transport
                && clew_core::linerpc::resolve_program(&launch.program).is_none()
            {
                return Err(format!(
                    "`{}` is not found on your PATH — give its full path",
                    launch.program
                ));
            }
        }
        for server in &servers {
            mcp::hub::Hub::global().forget(&server.name);
            mcp::config::save(server)?;
        }
        draft.new_mcp_name.clear();
        draft.new_mcp_target.clear();
        draft.new_mcp_extra.clear();
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        Ok(Some(format!(
            "Added MCP server {} — its tools join Ask's answers (calls ask first)",
            names.join(", ")
        )))
    }

    /// Add the ACP agent the form describes.
    fn add_acp_from_form(&mut self) -> Result<Option<String>, String> {
        let draft = &mut self.settings.integrations;
        let name = draft.new_acp_name.trim().to_string();
        if !mcp::config::valid_name(&name) {
            return Err("give the agent a name: letters, digits, - and _".into());
        }
        let mut words = split_command_line(draft.new_acp_command.trim())?;
        if words.is_empty() {
            return Err("give the command that starts the agent in ACP mode".into());
        }
        let program = words.remove(0);
        if clew_core::linerpc::resolve_program(&program).is_none() {
            return Err(format!(
                "`{program}` is not found on your PATH — give its full path"
            ));
        }
        acp::config::save(&acp::AgentConfig {
            name: name.clone(),
            launch: clew_core::linerpc::Launch {
                program,
                args: words,
                env: Vec::new(),
                cwd: None,
            },
            share_mcp: false,
        })?;
        draft.new_acp_name.clear();
        draft.new_acp_command.clear();
        Ok(Some(format!(
            "Added agent `{name}` — pick it in the Ask panel"
        )))
    }
}

/// What the id of the decline clew adds to a permission request starts as
/// ([`with_a_decline`]).
pub(crate) const DECLINE: &str = "clew:decline";

/// An ACP agent's answers to a permission request, and a way to decline
/// when it offers none (only allowing answers, or none at all) — the card
/// would otherwise leave the user nothing to say but yes, or nothing — with
/// the id it was given: one no answer of the agent's has. Chosen, the agent
/// hears a cancelled request (`Agent::answer_permission`).
pub(crate) fn with_a_decline(
    mut options: Vec<ApprovalOption>,
) -> (Vec<ApprovalOption>, Option<String>) {
    if options.iter().any(|o| !o.allows) {
        return (options, None);
    }
    let mut id = DECLINE.to_string();
    while options.iter().any(|o| o.id == id) {
        id.push('_');
    }
    options.push(ApprovalOption {
        id: id.clone(),
        label: "Decline".into(),
        allows: false,
    });
    (options, Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_like_a_shell() {
        assert_eq!(
            split_command_line(r#"npx -y "@scope/pkg" --dir 'a b' c\ d "x\"y""#).unwrap(),
            ["npx", "-y", "@scope/pkg", "--dir", "a b", "c d", "x\"y"]
        );
        assert_eq!(split_command_line("  ").unwrap(), Vec::<String>::new());
        assert_eq!(split_command_line("''").unwrap(), [""]);
        assert!(split_command_line("'open").is_err());
        assert!(split_command_line("\"open").is_err());
    }

    #[test]
    fn pairs_read_as_env_or_headers() {
        assert_eq!(
            parse_pairs("A=1; B = two\nAuthorization: Bearer x").unwrap(),
            [
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two".to_string()),
                ("Authorization".to_string(), "Bearer x".to_string()),
            ]
        );
        assert!(parse_pairs("").unwrap().is_empty());
        assert!(parse_pairs("nothing").is_err());
        assert!(parse_pairs("=x").is_err());
        // Split at whichever comes first.
        assert_eq!(
            parse_pairs("Authorization: Bearer abc==; PATH=/a:/b").unwrap(),
            [
                ("Authorization".to_string(), "Bearer abc==".to_string()),
                ("PATH".to_string(), "/a:/b".to_string()),
            ]
        );
        // An entry it cannot read is not repeated back.
        let err = parse_pairs("sk-secret-token").unwrap_err();
        assert!(!err.contains("sk-secret"), "{err}");
    }

    #[test]
    fn argument_previews_are_bounded() {
        assert_eq!(args_preview(&serde_json::json!({})), "(no arguments)");
        // A padded argument does not hide the one after it.
        let padded = serde_json::json!({ "a_pad": "x".repeat(5000), "command": "rm -rf ~" });
        let shown = args_preview(&padded);
        assert!(shown.contains("rm -rf ~"), "{shown}");
        assert!(shown.contains("4400 more characters"), "{shown}");
        // However much there is, the card says what it left out.
        let many: serde_json::Map<String, serde_json::Value> = (0..100)
            .map(|i| (format!("k{i:03}"), "y".repeat(1000).into()))
            .collect();
        let shown = args_preview(&serde_json::Value::Object(many));
        assert!(shown.chars().count() < MAX_DETAIL_CHARS + 200);
        assert!(shown.contains("more characters not shown"));
    }
}
