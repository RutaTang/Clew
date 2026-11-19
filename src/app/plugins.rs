//! Plugins in the window (docs/plugins.md): the ones installed, their
//! commands in the palette, running one on the open project and showing
//! what it gives back, asking the user before one is installed, Settings →
//! Plugins, and the plugin tools of Clew's own Ask agent — plus the requests
//! `clew-server mcp` files for the window when an ACP agent tries or
//! installs a plugin.
//!
//! Its messages, [`PluginMsg`], arrive through `App::update_plugins`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clew_plugin::runtime::{Failure, Invocation, Level, Limits, Loaded, Outcome, Runtime, View};
use clew_plugin::store::{Origin as Source, Record, Store};
use clew_plugin::{Inspected, Manifest};

use crate::app::model::Questions;
use crate::app::plugin_host::{HostEvent, ProjectHost};
use crate::app::prelude::*;
use crate::*;

/// The most log lines kept per plugin.
const LOG_LINES: usize = 200;

/// A plugin the user installed.
#[derive(Debug, Clone)]
pub struct Installed {
    pub(crate) id: String,
    pub(crate) record: Record,
    pub(crate) manifest: Manifest,
}

impl Installed {
    /// Its name, in the reader's language.
    pub(crate) fn name(&self) -> String {
        self.manifest
            .name_in(crate::i18n::current().code())
            .to_string()
    }

    /// Its description, in the reader's language.
    pub(crate) fn description(&self) -> String {
        self.manifest
            .description_in(crate::i18n::current().code())
            .to_string()
    }
}

/// A command's title, in the reader's language.
pub(crate) fn command_title(command: &clew_plugin::manifest::Command) -> String {
    command
        .translations
        .get(crate::i18n::current().code())
        .cloned()
        .unwrap_or_else(|| command.title.clone())
}

/// Who an install is answered to, besides the user: the agent that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Nobody: the user picked the file.
    None,
    /// The Ask agent's tool call `call` of turn `stream`.
    AskTool { stream: u64, call: u64 },
    /// The request `nonce` of `clew-server mcp`.
    Request(String),
}

/// A plugin waiting for the user to say whether to install it.
#[derive(Debug, Clone)]
pub struct PendingInstall {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) inspected: Inspected,
    pub(crate) origin: Source,
    /// The version installed now, if this replaces one.
    pub(crate) previous: Option<Installed>,
    pub(crate) reply: Reply,
}

impl PendingInstall {
    /// The permissions it needs that the version installed now does not
    /// have: what is new on the card.
    pub(crate) fn new_permissions(&self) -> u8 {
        let before = self.previous.as_ref().map_or(0, |p| p.record.granted());
        self.inspected.needs & !before
    }
}

/// A command asking for a line of text before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub(crate) id: String,
    pub(crate) command: String,
    /// The command's title, as the prompt says it.
    pub(crate) title: String,
    /// What it asks for.
    pub(crate) asks: String,
    pub(crate) text: String,
}

/// A page a plugin's command showed, over the code.
#[derive(Debug, Clone)]
pub struct PluginPage {
    pub(crate) id: String,
    /// The plugin's name, heading the page.
    pub(crate) plugin: String,
    pub(crate) view: View,
    /// Each Markdown block parsed, by its place in the view.
    pub(crate) markdown: HashMap<usize, Vec<iced::widget::markdown::Item>>,
    /// Each code block coloured, by its place in the view.
    pub(crate) code: HashMap<usize, Vec<crate::highlight::HlLine>>,
}

impl PluginPage {
    /// The page for `view`, its Markdown parsed and its code coloured once,
    /// here, rather than at every frame.
    pub(crate) fn new(id: String, plugin: String, view: View) -> Self {
        use clew_plugin::runtime::Block;
        let mut markdown = HashMap::new();
        let mut code = HashMap::new();
        for (i, block) in view.blocks.iter().enumerate() {
            match block {
                Block::Text(text) => {
                    markdown.insert(i, iced::widget::markdown::parse(text).collect());
                }
                Block::Code(c) => {
                    let lang = c
                        .language
                        .as_deref()
                        .and_then(crate::highlight::lang_for_fence);
                    code.insert(i, crate::highlight::highlight_lines(&c.text, lang));
                }
                _ => {}
            }
        }
        PluginPage {
            id,
            plugin,
            view,
            markdown,
            code,
        }
    }
}

/// The plugins' state in the window.
#[derive(Default)]
pub struct Plugins {
    /// Made when a plugin first runs.
    runtime: Option<Arc<Runtime>>,
    pub(crate) installed: Vec<Installed>,
    /// Plugins ready to run, by their file's fingerprint.
    loaded: HashMap<String, Arc<Loaded>>,
    /// The command running, by its title, as the status bar says it.
    pub(crate) running: Option<String>,
    pub(crate) install: Questions<PendingInstall>,
    pub(crate) prompt: Option<Prompt>,
    /// What each plugin wrote to its log, by id, the latest last.
    pub(crate) log: HashMap<String, VecDeque<(Level, String)>>,
    /// Why the last install from a file, or the last change, failed.
    pub(crate) failure: Option<String>,
    /// The requests of `clew-server mcp` this window took on.
    handling: HashSet<String>,
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugins")
            .field("installed", &self.installed.len())
            .field("loaded", &self.loaded.len())
            .field("running", &self.running)
            .field("install", &self.install.len())
            .finish_non_exhaustive()
    }
}

impl Plugins {
    /// The window's plugins: the ones installed.
    pub(crate) fn new() -> Self {
        Plugins {
            installed: Self::read_installed(),
            ..Default::default()
        }
    }

    /// The plugins installed, as `plugins.toml` lists them.
    pub(crate) fn read_installed() -> Vec<Installed> {
        let Some(data) = clew_core::lsp::store::data_root() else {
            return Vec::new();
        };
        let store = Store::load(&data);
        let mut installed: Vec<Installed> = store
            .plugin
            .into_iter()
            .filter_map(|(id, record)| {
                let manifest = record.manifest()?;
                Some(Installed {
                    id,
                    record,
                    manifest,
                })
            })
            .collect();
        installed.sort_by_key(|p| p.name().to_lowercase());
        installed
    }

    /// The installed plugin `id`.
    pub(crate) fn get(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|p| p.id == id)
    }

    /// Note `line` in plugin `id`'s log.
    fn note(&mut self, id: &str, level: Level, line: String) {
        let log = self.log.entry(id.to_string()).or_default();
        log.push_back((level, line));
        while log.len() > LOG_LINES {
            log.pop_front();
        }
    }
}

/// What came of a run.
pub struct Ran {
    pub(crate) outcome: Result<Outcome, Failure>,
    /// The plugin, ready to run again, by its fingerprint.
    pub(crate) loaded: Option<(String, Arc<Loaded>)>,
    pub(crate) log: Vec<HostEvent>,
}

impl std::fmt::Debug for Ran {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ran")
            .field("outcome", &self.outcome)
            .field("loaded", &self.loaded.as_ref().map(|(fp, _)| fp))
            .field("log", &self.log.len())
            .finish()
    }
}

/// A plugin file read and inspected, ready to be asked about.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub(crate) bytes: Arc<Vec<u8>>,
    pub(crate) inspected: Inspected,
}

/// A plugin's message.
#[derive(Debug, Clone)]
pub enum PluginMsg {
    /// Run plugin `id`'s command `command` (the palette, voice, the agent).
    Run {
        id: String,
        command: String,
    },
    /// The text typed for a command that asks for some.
    PromptEdit(String),
    /// Run the command with what was typed.
    PromptRun,
    PromptCancel,
    /// A run ended.
    Ran {
        stamp: Stamp,
        id: String,
        title: String,
        ran: Arc<std::sync::Mutex<Option<Ran>>>,
    },
    /// Open the place of row `row` of block `block` of the page shown.
    Open {
        block: usize,
        row: usize,
    },
    /// Put the page away.
    ClosePage,
    /// Settings: install a plugin from a file.
    InstallFromFile,
    Picked(Option<PathBuf>),
    /// A file read for installing, and where from.
    Read {
        result: Arc<Result<Prepared, String>>,
        origin: Source,
        reply: Reply,
    },
    InstallAllowed(u64),
    InstallDismissed(u64),
    /// An install written, or why not.
    Installed {
        id: String,
        reply: Reply,
        result: Result<String, String>,
    },
    SetEnabled(String, bool),
    Remove(String),
    /// Requests `clew-server mcp` filed, as [`requests_subscription`] found
    /// them.
    Requests(Arc<Vec<(String, clew_plugin::requests::Request)>>),
    /// An Ask agent's plugin tool call has its answer.
    ToolAnswered {
        stream: u64,
        call: u64,
        answer: Arc<std::sync::Mutex<Option<clew_plugin::tools::Step>>>,
    },
    /// A try for an agent ran: the answer for the agent.
    Tried {
        reply: Reply,
        answer: Result<String, String>,
    },
}

impl PluginMsg {
    /// Where the message comes from, for `dispatch`'s ownership check.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            PluginMsg::Ran { stamp, .. } => Some(Origin::Stamped(stamp)),
            _ => None,
        }
    }
}

/// The data folder, or why there is none.
fn data() -> Result<PathBuf, String> {
    clew_core::lsp::store::data_root().ok_or_else(|| tr!("Clew has no data folder").to_string())
}

/// A failure, in the reader's words.
fn failure_words(failure: &Failure) -> String {
    match failure {
        // The plugin's own words, as it wrote them.
        Failure::Plugin(why) => why.clone(),
        Failure::Stopped(why) => crate::i18n::lookup(why).to_string(),
    }
}

impl App {
    /// The plugin runtime, made when first wanted.
    fn plugin_runtime(&mut self) -> Result<Arc<Runtime>, String> {
        if let Some(runtime) = &self.plugins.runtime {
            return Ok(runtime.clone());
        }
        let runtime = Arc::new(Runtime::new(clew_core::lsp::store::data_root())?);
        self.plugins.runtime = Some(runtime.clone());
        Ok(runtime)
    }

    /// Read the installed plugins again (after an install, a change).
    pub(crate) fn reload_plugins(&mut self) {
        self.plugins.installed = Plugins::read_installed();
        let keep: HashSet<&str> = self
            .plugins
            .installed
            .iter()
            .filter(|p| p.record.enabled)
            .map(|p| p.record.fingerprint.as_str())
            .collect();
        self.plugins
            .loaded
            .retain(|fp, _| keep.contains(fp.as_str()));
        if let Some(runtime) = &self.plugins.runtime {
            let keep: Vec<String> = self
                .plugins
                .installed
                .iter()
                .map(|p| p.record.sha256.clone())
                .collect();
            runtime.sweep_compiled(&keep);
        }
    }

    /// The open project, as plugins see it; `None` with none open.
    fn project_host(
        &self,
    ) -> Option<Box<dyn FnOnce(tokio::runtime::Handle) -> ProjectHost + Send>> {
        let project = self.proj.project.as_ref()?;
        let name = project
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let files = project.files.clone();
        let partial = project.truncated;
        if self.local_project_state() {
            let root = project.root.clone();
            Some(Box::new(move |_| {
                ProjectHost::local(root, name, files, partial)
            }))
        } else {
            let ai = self.ai_client();
            Some(Box::new(move |handle| {
                ProjectHost::remote(ai, handle, name, files, partial)
            }))
        }
    }

    /// Where a command is run from: the file shown, the caret, the selection.
    fn invocation(&self, input: Option<String>) -> Invocation {
        let viewer = self.active_viewer();
        Invocation {
            file: viewer.map(|v| v.rel.clone()).filter(|r| !r.is_empty()),
            line: viewer
                .and_then(|v| v.caret)
                .map(|(line, _)| (line + 1).min(u32::MAX as usize) as u32),
            selection: viewer.and_then(|v| v.selected_text()),
            input,
        }
    }

    /// Run plugin `id`'s command `command`, asking first for what it asks.
    fn run_plugin(&mut self, id: &str, command: &str, input: Option<String>) -> Task<Message> {
        let Some(plugin) = self.plugins.get(id).filter(|p| p.record.enabled).cloned() else {
            self.status = tr!("That plugin is not installed, or is turned off").into();
            return Task::none();
        };
        let Some(cmd) = plugin.manifest.command(command).cloned() else {
            self.status = tr!("{plugin} has no such command", plugin = plugin.name());
            return Task::none();
        };
        let title = command_title(&cmd);
        if let (Some(asks), None) = (&cmd.input, &input) {
            self.plugins.prompt = Some(Prompt {
                id: id.to_string(),
                command: command.to_string(),
                title,
                asks: asks.clone(),
                text: String::new(),
            });
            return iced::widget::operation::focus(crate::ui::plugin_prompt_id());
        }
        if let Some(running) = &self.plugins.running {
            self.status = tr!("{command} is still running", command = running.clone());
            return Task::none();
        }
        let Some(host) = self.project_host() else {
            self.status = tr!("Open a project to run {command}", command = title);
            return Task::none();
        };
        let runtime = match self.plugin_runtime() {
            Ok(runtime) => runtime,
            Err(why) => {
                self.status = tr!("Plugins cannot run: {reason}", reason = why);
                return Task::none();
            }
        };
        let data = match data() {
            Ok(data) => data,
            Err(why) => {
                self.status = why;
                return Task::none();
            }
        };
        let invocation = self.invocation(input);
        let loaded = self.plugins.loaded.get(&plugin.record.fingerprint).cloned();
        self.plugins.running = Some(title.clone());
        self.status = tr!("Running {command}…", command = title.clone());
        let stamp = self.stamp();
        let id = id.to_string();
        let command = command.to_string();
        let record = plugin.record.clone();
        let done_id = id.clone();
        let done_title = title.clone();
        Task::perform(
            async move {
                let handle = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || {
                    let host = Arc::new(host(handle));
                    let (loaded, fresh) = match loaded {
                        Some(loaded) => (loaded, false),
                        None => match load(&runtime, &data, &record) {
                            Ok(loaded) => (Arc::new(loaded), true),
                            Err(why) => {
                                return Ran {
                                    outcome: Err(Failure::Stopped(why)),
                                    loaded: None,
                                    log: Vec::new(),
                                };
                            }
                        },
                    };
                    let outcome = loaded.run(host.clone(), &command, &invocation, Limits::COMMAND);
                    Ran {
                        outcome,
                        loaded: fresh.then(|| (record.fingerprint.clone(), loaded)),
                        log: host.take_events(),
                    }
                })
                .await
                .unwrap_or_else(|_| Ran {
                    outcome: Err(Failure::Stopped(
                        tr!("the plugin's run failed unexpectedly").to_string(),
                    )),
                    loaded: None,
                    log: Vec::new(),
                })
            },
            move |ran| {
                Message::Plugin(PluginMsg::Ran {
                    stamp: stamp.clone(),
                    id: done_id.clone(),
                    title: done_title.clone(),
                    ran: Arc::new(std::sync::Mutex::new(Some(ran))),
                })
            },
        )
    }

    /// A run's end: show what it gave back.
    fn on_plugin_ran(&mut self, id: String, title: String, ran: Ran) -> Task<Message> {
        self.plugins.running = None;
        self.settle_status(&[&tr!("Running {command}…", command = title.clone())]);
        for HostEvent::Log(level, line) in ran.log {
            self.plugins.note(&id, level, line);
        }
        if let Some((fingerprint, loaded)) = ran.loaded {
            self.plugins.loaded.insert(fingerprint, loaded);
        }
        let name = self
            .plugins
            .get(&id)
            .map(Installed::name)
            .unwrap_or_default();
        match ran.outcome {
            Ok(Outcome::Show(view)) => {
                self.show_plugin_page(PluginPage::new(id, name, view));
                Task::none()
            }
            Ok(Outcome::Open(at)) => {
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let line = (at.line > 0).then_some(at.line as usize);
                self.open_file(root.join(&at.path), line, true)
            }
            Ok(Outcome::Say(text)) => {
                self.status = format!("{title}: {text}");
                Task::none()
            }
            Ok(Outcome::Nothing) => {
                self.status = tr!("{command} is done", command = title);
                Task::none()
            }
            Err(failure) => {
                let why = failure_words(&failure);
                self.plugins.note(&id, Level::Error, why.clone());
                self.status = tr!(
                    "{command} did not finish: {reason}",
                    command = title,
                    reason = why
                );
                Task::none()
            }
        }
    }

    /// Show `page` over the code, in place of any other page.
    pub(crate) fn show_plugin_page(&mut self, page: PluginPage) {
        self.proj.overview.showing = false;
        self.proj.stats.showing = false;
        self.proj.weak.showing = false;
        self.proj.glossary.showing = false;
        self.proj.docs.page = None;
        self.proj.plugin_page = Some(page);
    }

    /// Ask the user whether to install `prepared`.
    fn offer_install(&mut self, prepared: Prepared, origin: Source, reply: Reply) {
        let previous = self
            .plugins
            .get(&prepared.inspected.manifest.plugin.id)
            .cloned();
        self.plugins.install.offer(PendingInstall {
            bytes: prepared.bytes,
            inspected: prepared.inspected,
            origin,
            previous,
            reply,
        });
    }

    /// Tell whoever asked for an install how it went.
    fn answer_install(&mut self, reply: &Reply, answer: Result<String, String>, declined: bool) {
        match reply {
            Reply::None => {}
            Reply::AskTool { stream, call } => {
                let (content, is_error) = match (&answer, declined) {
                    (_, true) => ("The user chose not to install it.".to_string(), false),
                    (Ok(what), _) => (what.clone(), false),
                    (Err(why), _) => (why.clone(), true),
                };
                let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                    stream: *stream,
                    call: *call,
                    content,
                    is_error,
                });
            }
            Reply::Request(nonce) => {
                let answer = match (answer, declined) {
                    (_, true) => clew_plugin::requests::Answer::Declined,
                    (Ok(fingerprint), _) => {
                        clew_plugin::requests::Answer::Installed { fingerprint }
                    }
                    (Err(why), _) => clew_plugin::requests::Answer::Failed { why },
                };
                if let Ok(data) = data() {
                    let _ = clew_plugin::requests::answer(&data, nonce, &answer);
                }
                self.plugins.handling.remove(nonce);
            }
        }
    }

    /// Run `built`'s command for an agent trying it: with the permissions
    /// its code needs, read and git at most, on the open project.
    fn try_for_agent(
        &mut self,
        built: clew_plugin::build::Built,
        command: String,
        file: Option<String>,
        line: Option<u32>,
        input: Option<String>,
        reply: Reply,
    ) -> Task<Message> {
        let Some(host) = self.project_host() else {
            return Task::done(Message::Plugin(PluginMsg::Tried {
                reply,
                answer: Err("No project is open in Clew to try it on.".into()),
            }));
        };
        let runtime = match self.plugin_runtime() {
            Ok(runtime) => runtime,
            Err(why) => {
                return Task::done(Message::Plugin(PluginMsg::Tried {
                    reply,
                    answer: Err(why),
                }));
            }
        };
        let invocation = Invocation {
            file,
            line,
            selection: None,
            input,
        };
        let granted = built.inspected.needs & (clew_rules::plugin::READ | clew_rules::plugin::GIT);
        Task::perform(
            async move {
                let handle = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || {
                    let host = Arc::new(host(handle));
                    let loaded = runtime.load(&built.bytes, &built.inspected, granted)?;
                    let outcome = loaded.run(host.clone(), &command, &invocation, Limits::COMMAND);
                    let log: Vec<String> = host
                        .take_events()
                        .into_iter()
                        .map(|HostEvent::Log(level, line)| format!("[{level:?}] {line}"))
                        .collect();
                    let mut answer = match outcome {
                        Ok(outcome) => clew_plugin::tools::describe_outcome(&outcome),
                        Err(Failure::Plugin(why)) => format!("It answered with an error: {why}"),
                        Err(Failure::Stopped(why)) => format!("It stopped: {why}"),
                    };
                    if !log.is_empty() {
                        answer.push_str("\n\nIts log:\n");
                        answer.push_str(&log.join("\n"));
                    }
                    Ok(answer)
                })
                .await
                .unwrap_or_else(|_| Err("the try failed unexpectedly".into()))
            },
            move |answer| {
                Message::Plugin(PluginMsg::Tried {
                    reply: reply.clone(),
                    answer,
                })
            },
        )
    }

    /// An agent turn's call of a plugin tool (`clew_plugin::tools`).
    pub(crate) fn on_plugin_tool(
        &mut self,
        stream: u64,
        call: u64,
        tool: String,
        args: serde_json::Value,
    ) -> Task<Message> {
        let data = match data() {
            Ok(data) => data,
            Err(why) => {
                let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                    stream,
                    call,
                    content: why,
                    is_error: true,
                });
                return Task::none();
            }
        };
        self.push_plugin_step(stream, &tool);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    clew_plugin::tools::call(&data, &tool, &args, &AtomicBool::new(false))
                })
                .await
                .unwrap_or_else(|_| {
                    clew_plugin::tools::Step::Done(Err("the tool failed unexpectedly".into()))
                })
            },
            move |step| {
                Message::Plugin(PluginMsg::ToolAnswered {
                    stream,
                    call,
                    answer: Arc::new(std::sync::Mutex::new(Some(step))),
                })
            },
        )
    }

    /// A step chip for a plugin tool the agent used.
    fn push_plugin_step(&mut self, stream: u64, tool: &str) {
        let title = match tool {
            "plugin_guide" => tr!("Read the plugin guide"),
            "plugin_new" => tr!("Started a plugin"),
            "plugin_files" | "plugin_read" => tr!("Read a plugin's source"),
            "plugin_write" => tr!("Wrote a plugin's source"),
            "plugin_build" => tr!("Built a plugin"),
            "plugin_try" => tr!("Tried a plugin"),
            "plugin_install" => tr!("Asked to install a plugin"),
            _ => tr!("Looked at the plugins"),
        };
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream) {
            turn.steps.push(crate::app::model::AgentStep {
                tool: "plugin".into(),
                title: title.to_string(),
                refs: Vec::new(),
                state: crate::app::model::StepState::Done,
            });
        }
    }

    /// Handle the requests found: try, or ask about installing.
    fn on_requests(&mut self, found: &[(String, clew_plugin::requests::Request)]) -> Task<Message> {
        use clew_plugin::requests::{Answer, Ask};
        let Ok(data) = data() else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        for (nonce, request) in found {
            // A try is for the window with its project open: another window
            // leaves it for that one.
            if let Ask::Try { root, .. } = &request.ask {
                let open = self
                    .proj
                    .project
                    .as_ref()
                    .is_some_and(|p| p.root.to_string_lossy() == *root);
                if !open {
                    continue;
                }
            }
            if self.plugins.handling.contains(nonce) || !clew_plugin::requests::claim(&data, nonce)
            {
                continue;
            }
            self.plugins.handling.insert(nonce.clone());
            let reply = Reply::Request(nonce.clone());
            let (plugin, sha256) = match &request.ask {
                Ask::Try { plugin, sha256, .. } | Ask::Install { plugin, sha256 } => {
                    (plugin, sha256)
                }
            };
            // The build named by its digest, from Clew's own build folder.
            let built = clew_plugin::tools::last_build(&data, plugin).and_then(|built| {
                if clew_plugin::inspect::hex(&built.inspected.sha256) == *sha256 {
                    Ok(built)
                } else {
                    Err("the plugin was built again since; ask again".to_string())
                }
            });
            let built = match built {
                Ok(built) => built,
                Err(why) => {
                    let _ = clew_plugin::requests::answer(&data, nonce, &Answer::Failed { why });
                    self.plugins.handling.remove(nonce);
                    continue;
                }
            };
            match &request.ask {
                Ask::Try {
                    command,
                    file,
                    line,
                    input,
                    ..
                } => {
                    tasks.push(self.try_for_agent(
                        built,
                        command.clone(),
                        file.clone(),
                        *line,
                        input.clone(),
                        reply,
                    ));
                }
                Ask::Install { .. } => {
                    self.offer_install(
                        Prepared {
                            bytes: Arc::new(built.bytes),
                            inspected: built.inspected,
                        },
                        Source::Agent(request.agent.clone()),
                        reply,
                    );
                }
            }
        }
        Task::batch(tasks)
    }

    /// Handle a [`PluginMsg`].
    pub(crate) fn update_plugins(&mut self, message: PluginMsg) -> Task<Message> {
        match message {
            PluginMsg::Run { id, command } => self.run_plugin(&id, &command, None),
            PluginMsg::PromptEdit(text) => {
                if let Some(prompt) = self.plugins.prompt.as_mut() {
                    prompt.text = text;
                }
                Task::none()
            }
            PluginMsg::PromptRun => {
                let Some(prompt) = self.plugins.prompt.take() else {
                    return Task::none();
                };
                self.run_plugin(&prompt.id, &prompt.command, Some(prompt.text))
            }
            PluginMsg::PromptCancel => {
                self.plugins.prompt = None;
                Task::none()
            }
            PluginMsg::Ran { id, title, ran, .. } => {
                let Some(ran) = ran.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    return Task::none();
                };
                self.on_plugin_ran(id, title, ran)
            }
            PluginMsg::Open { block, row } => {
                let at = self
                    .proj
                    .plugin_page
                    .as_ref()
                    .and_then(|page| crate::ui::plugin_row_place(&page.view, block, row));
                let (Some(at), Some(root)) =
                    (at, self.proj.project.as_ref().map(|p| p.root.clone()))
                else {
                    return Task::none();
                };
                let line = (at.line > 0).then_some(at.line as usize);
                // Opening the file puts the page away, as it does every page.
                self.open_file(root.join(&at.path), line, true)
            }
            PluginMsg::ClosePage => {
                self.proj.plugin_page = None;
                Task::none()
            }
            PluginMsg::InstallFromFile => {
                self.plugins.failure = None;
                crate::app::tasks::pick_file(self.main_window, tr!("Install a plugin").into()).map(
                    |picked| match picked {
                        Ok(path) => Message::Plugin(PluginMsg::Picked(path)),
                        Err(_) => Message::Window(WindowMsg::NoFileDialog),
                    },
                )
            }
            PluginMsg::Picked(None) => Task::none(),
            PluginMsg::Picked(Some(path)) => {
                let origin = Source::File(path.to_string_lossy().into_owned());
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || -> Result<Prepared, String> {
                            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                            let inspected = clew_plugin::inspect(&bytes)?;
                            Ok(Prepared {
                                bytes: Arc::new(bytes),
                                inspected,
                            })
                        })
                        .await
                        .unwrap_or_else(|_| Err("the file could not be read".into()))
                    },
                    move |result| {
                        Message::Plugin(PluginMsg::Read {
                            result: Arc::new(result),
                            origin: origin.clone(),
                            reply: Reply::None,
                        })
                    },
                )
            }
            PluginMsg::Read {
                result,
                origin,
                reply,
            } => {
                match result.as_ref() {
                    Ok(prepared) => self.offer_install(prepared.clone(), origin, reply),
                    Err(why) => {
                        let why = crate::i18n::lookup(why).to_string();
                        self.plugins.failure = Some(tr!(
                            "That file is not a plugin Clew can install: {reason}",
                            reason = why.clone()
                        ));
                        self.answer_install(&reply, Err(why), false);
                    }
                }
                Task::none()
            }
            PluginMsg::InstallAllowed(number) => {
                if !self.may_allow(&crate::app::model::Question::Plugin(number)) {
                    return Task::none();
                }
                let Some(pending) = self.plugins.install.answer(number) else {
                    return Task::none();
                };
                let data = match data() {
                    Ok(data) => data,
                    Err(why) => {
                        self.answer_install(&pending.reply, Err(why), false);
                        return Task::none();
                    }
                };
                let id = pending.inspected.manifest.plugin.id.clone();
                let reply = pending.reply.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            clew_plugin::store::install(
                                &data,
                                &pending.bytes,
                                &pending.inspected,
                                pending.origin,
                            )
                            .map(|record| record.fingerprint)
                        })
                        .await
                        .unwrap_or_else(|_| Err("the install failed unexpectedly".into()))
                    },
                    move |result| {
                        Message::Plugin(PluginMsg::Installed {
                            id: id.clone(),
                            reply: reply.clone(),
                            result,
                        })
                    },
                )
            }
            PluginMsg::InstallDismissed(number) => {
                if let Some(pending) = self.plugins.install.answer(number) {
                    self.answer_install(&pending.reply, Err(String::new()), true);
                }
                Task::none()
            }
            PluginMsg::Installed { id, reply, result } => {
                self.reload_plugins();
                let name = self
                    .plugins
                    .get(&id)
                    .map(Installed::name)
                    .unwrap_or(id.clone());
                let answer = match result {
                    Ok(fingerprint) => {
                        self.status = tr!(
                            "{plugin} is installed: its commands are in the command palette",
                            plugin = name.clone()
                        );
                        let commands = self
                            .plugins
                            .get(&id)
                            .map(|p| {
                                p.manifest
                                    .commands
                                    .iter()
                                    .map(|c| format!("\"{}\"", c.title))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        let _ = fingerprint;
                        Ok(format!(
                            "The user installed {name}. Its commands are in Clew's command palette: {commands}. You can run one with run_command."
                        ))
                    }
                    Err(why) => {
                        self.plugins.failure = Some(tr!(
                            "{plugin} could not be installed: {reason}",
                            plugin = name.clone(),
                            reason = why.clone()
                        ));
                        Err(why)
                    }
                };
                self.answer_install(&reply, answer, false);
                Task::none()
            }
            PluginMsg::SetEnabled(id, on) => {
                if let Err(why) = data().and_then(|d| clew_plugin::store::set_enabled(&d, &id, on))
                {
                    self.plugins.failure = Some(why);
                }
                self.reload_plugins();
                Task::none()
            }
            PluginMsg::Remove(id) => {
                let removed = data().and_then(|d| {
                    clew_plugin::store::remove(&d, &id)?;
                    let _ = std::fs::remove_dir_all(clew_plugin::runtime::store_dir(&d, &id));
                    Ok(())
                });
                if let Err(why) = removed {
                    self.plugins.failure = Some(why);
                }
                if self.proj.plugin_page.as_ref().is_some_and(|p| p.id == id) {
                    self.proj.plugin_page = None;
                }
                self.plugins.log.remove(&id);
                self.reload_plugins();
                Task::none()
            }
            PluginMsg::Requests(found) => self.on_requests(&found),
            PluginMsg::ToolAnswered {
                stream,
                call,
                answer,
            } => {
                let Some(step) = answer.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    return Task::none();
                };
                let reply = Reply::AskTool { stream, call };
                match step {
                    clew_plugin::tools::Step::Done(result) => {
                        let (content, is_error) = match result {
                            Ok(text) => (text, false),
                            Err(why) => (why, true),
                        };
                        let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                            stream,
                            call,
                            content,
                            is_error,
                        });
                        // A build or a write may have changed what is
                        // listed; an install changes it only once allowed.
                        Task::none()
                    }
                    clew_plugin::tools::Step::Try {
                        built,
                        command,
                        file,
                        line,
                        input,
                    } => self.try_for_agent(*built, command, file, line, input, reply),
                    clew_plugin::tools::Step::Install { built } => {
                        self.offer_install(
                            Prepared {
                                bytes: Arc::new(built.bytes),
                                inspected: built.inspected,
                            },
                            Source::Ask,
                            reply,
                        );
                        Task::none()
                    }
                }
            }
            PluginMsg::Tried { reply, answer } => {
                match &reply {
                    Reply::AskTool { stream, call } => {
                        let (content, is_error) = match answer {
                            Ok(text) => (text, false),
                            Err(why) => (why, true),
                        };
                        let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                            stream: *stream,
                            call: *call,
                            content,
                            is_error,
                        });
                    }
                    Reply::Request(nonce) => {
                        if let Ok(data) = data() {
                            let answer = match answer {
                                Ok(result) => clew_plugin::requests::Answer::Tried { result },
                                Err(why) => clew_plugin::requests::Answer::Failed { why },
                            };
                            let _ = clew_plugin::requests::answer(&data, nonce, &answer);
                        }
                        self.plugins.handling.remove(nonce);
                    }
                    Reply::None => {}
                }
                Task::none()
            }
        }
    }
}

/// Load the approved copy of `record`'s plugin, checked, and link it with
/// what was granted.
fn load(runtime: &Runtime, data: &std::path::Path, record: &Record) -> Result<Loaded, String> {
    let bytes = clew_plugin::store::read_approved(data, record)?;
    let inspected = clew_plugin::inspect(&bytes)?;
    runtime.load(&bytes, &inspected, record.granted())
}

/// The ids of a turn's plugin tools, offered to the Ask agent beside the
/// window's (`clew_plugin::tools`).
pub(crate) fn plugin_tools() -> Vec<clew_protocol::ExternalTool> {
    clew_plugin::tools::tool_defs()
        .into_iter()
        .map(|t| clew_protocol::ExternalTool {
            name: t.name.into(),
            description: format!("[Clew plugins] {}", t.description),
            parameters: t.schema,
        })
        .collect()
}

/// Watch for requests `clew-server mcp` files for the window, off the
/// window's thread: the window hears of them only when the set waiting
/// changes, so an idle window is not woken.
pub(crate) fn requests_subscription(data: PathBuf) -> Subscription<Message> {
    Subscription::run_with(data, watch_requests)
}

/// How often the requests folder is looked at.
const REQUESTS_EVERY: std::time::Duration = std::time::Duration::from_millis(700);

fn watch_requests(data: &PathBuf) -> impl iced::futures::Stream<Item = Message> + use<> {
    let data = data.clone();
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            let mut seen: Vec<String> = Vec::new();
            loop {
                tokio::time::sleep(REQUESTS_EVERY).await;
                let looking = data.clone();
                let found =
                    tokio::task::spawn_blocking(move || clew_plugin::requests::pending(&looking))
                        .await
                        .unwrap_or_default();
                let nonces: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
                if nonces == seen {
                    continue;
                }
                seen = nonces;
                if found.is_empty() {
                    continue;
                }
                let message = Message::Plugin(PluginMsg::Requests(Arc::new(found)));
                if output.send(message).await.is_err() {
                    break;
                }
            }
        },
    )
}
