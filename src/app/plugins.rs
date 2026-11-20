//! Plugins in the window (docs/plugins.md): the ones installed, their
//! commands in the palette, running one on the open project and showing
//! what it gives back, asking the user before one is installed, Settings →
//! Plugins, and the plugin tools of Clew's own Ask agent — plus the requests
//! `clew-server mcp` files for the window when an ACP agent tries or
//! installs a plugin.
//!
//! Its messages, [`PluginMsg`], arrive through `App::update_plugins`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clew_plugin::runtime::{
    Failure, Graph, Invocation, Level, Limits, Loaded, Outcome, Runtime, View,
};
use clew_plugin::store::{Origin as Source, Record, Store};
use clew_plugin::{Inspected, Manifest};

use crate::app::model::Questions;
use crate::app::plugin_host::{HostEvent, ProjectHost};
use crate::app::prelude::*;
use crate::app::state::FailureOf;
use crate::*;

/// The most log lines kept per plugin.
const LOG_LINES: usize = 200;

/// A plugin the user installed.
#[derive(Debug, Clone)]
pub struct Installed {
    pub(crate) id: String,
    pub(crate) record: Record,
    pub(crate) manifest: Manifest,
    /// Whether its source is among the agents' plugin sources
    /// (`authoring::source_dir`): there, an agent can change it.
    pub(crate) has_source: bool,
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

/// How a command is named where Clew lists it, in the reader's language:
/// "{plugin}: {command}", or the one name when both are the same (a
/// plugin of one command often names it as itself: "Word count: Word
/// count" said it twice).
pub(crate) fn command_label(
    plugin: &Installed,
    command: &clew_plugin::manifest::Command,
) -> String {
    let name = plugin.name();
    let title = command_title(command);
    if title == name {
        name
    } else {
        format!("{name}: {title}")
    }
}

/// An enabled plugin's command where Clew offers it: the palette, the
/// code's menu.
pub(crate) struct Offered<'a> {
    pub(crate) plugin: &'a Installed,
    pub(crate) command: &'a clew_plugin::manifest::Command,
    /// Its name in English, by which the palette, the agent's
    /// `run_command` and the voice run it.
    pub(crate) title: String,
    /// Its name as shown: [`command_label`], with "…" when it asks for
    /// something first, as Clew's own commands that open a question are
    /// named.
    pub(crate) shown: String,
}

/// The enabled plugins' commands, each with a name no other command has:
/// the plugin's name and the command's, and, where that names two
/// commands alike — two plugins', or a plugin's and one of Clew's own
/// (`clews`, each its English title and the name its row shows) — its
/// plugin's id after it, which no other plugin has (a plugin's own
/// commands are titled apart). Run by name, the second of two alike ran
/// the first. Alike in English, by which they are run, both names say the
/// id; alike only as shown in the reader's language, the row says it.
pub(crate) fn offered_commands<'a>(
    installed: &'a [Installed],
    clews: &[(&str, &str)],
) -> Vec<Offered<'a>> {
    let mut offered: Vec<Offered> = installed
        .iter()
        .filter(|p| p.record.enabled)
        .flat_map(|plugin| {
            plugin.manifest.commands.iter().map(move |command| Offered {
                plugin,
                command,
                title: plugin.manifest.palette_title(command),
                shown: command_label(plugin, command),
            })
        })
        .collect();
    // Read alike as the voice reads titles — by their words, whatever their
    // case or what stands between them — in English, and as shown.
    let bare = |name: &str| alike_key(name);
    let count = |names: Vec<String>| {
        let mut named: HashMap<String, usize> = HashMap::new();
        for name in names {
            *named.entry(name).or_default() += 1;
        }
        named
    };
    // Read again once ids are added, until none is alike: a title made
    // alike by another's id — a plugin titling its command "Count words
    // (word-count)" — gains its own too, so no two are ever named alike,
    // and none is taken for another's by the id it wrote.
    let mut tagged = vec![(false, false); offered.len()];
    loop {
        let titles = count(
            clews
                .iter()
                .map(|(title, _)| bare(title))
                .chain(offered.iter().map(|o| bare(&o.title)))
                .collect(),
        );
        let shown = count(
            clews
                .iter()
                .map(|(_, shown)| bare(shown))
                .chain(offered.iter().map(|o| bare(&o.shown)))
                .collect(),
        );
        let mut added = false;
        for (o, (title_has, shown_has)) in offered.iter_mut().zip(&mut tagged) {
            let id = &o.plugin.id;
            let titled_alike = !*title_has && titles.get(&bare(&o.title)).is_some_and(|n| *n > 1);
            if titled_alike {
                o.title = format!("{} ({id})", o.title);
                *title_has = true;
                added = true;
            }
            if !*shown_has && (titled_alike || shown.get(&bare(&o.shown)).is_some_and(|n| *n > 1)) {
                o.shown = format!("{} ({id})", o.shown);
                *shown_has = true;
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    // The id is part of its name; what it asks for comes after.
    for o in &mut offered {
        if o.command.input.is_some() {
            o.shown.push('…');
        }
    }
    offered
}

/// A command's name as the voice reads it, to tell names alike: its words,
/// lowercase, whatever stands between them ("Call-Graph", "call graph…").
pub(crate) fn alike_key(name: &str) -> String {
    clew_voice::intent::plain_words(name)
}

/// An install done, as whoever asked for it is told: its fingerprint, in
/// words for an agent, and its commands by the names the palette gives
/// them (in the order of its `[[command]]`s).
pub(crate) struct Installation {
    pub(crate) fingerprint: String,
    pub(crate) words: String,
    pub(crate) palette: Vec<String>,
}

/// A command as it was run — its plugin, what was typed for it, where —
/// so a failed run's Try again runs the same again.
#[derive(Debug, Clone, PartialEq)]
pub struct Again {
    pub(crate) id: String,
    pub(crate) command: String,
    pub(crate) input: Option<String>,
    pub(crate) at: Option<Spot>,
    /// The file, line and selection it was run on, once it ran: run again
    /// on them, not on whatever file a row opened since.
    pub(crate) context: Option<Invocation>,
}

/// Where a command run from the code's right-click menu was asked: the
/// pane and the line clicked, which the caret need not be on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spot {
    pub(crate) pane: usize,
    /// From 0.
    pub(crate) line: usize,
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
    /// The name of the plugin this replaces that this Clew cannot read (a
    /// later Clew installed it), if it replaces one: the card said
    /// "Install", and the later build went with no word.
    pub(crate) unreadable: Option<String>,
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

/// What a plugin graph built to, for which files and which copy of the
/// plugin. A graph built is kept while it is built again.
#[derive(Debug, Clone, Default)]
pub struct PluginMap {
    /// The file registry's revision it was asked for at.
    pub(crate) rev: u64,
    /// The copy of the plugin it was asked of.
    pub(crate) fingerprint: String,
    pub(crate) building: bool,
    /// The last graph it built.
    pub(crate) built: Option<clew_protocol::ProjectMap>,
    /// Why the last build gave nothing, in the reader's language.
    pub(crate) failed: Option<String>,
    /// What the plugin answered in its own words instead of a graph ("This
    /// project has no Kafka topics"): its answer, not a failure.
    pub(crate) said: Option<String>,
    /// Set to stop the build running: a graph no longer shown is not built
    /// on for a minute, with its memory, beside the one that is.
    pub(crate) stop: Option<Arc<AtomicBool>>,
    /// Which build was started last ([`next_build`]): only its result is
    /// this graph's. One stopped as it ended gave a result too, after the
    /// next had started, and ended that one in its place.
    pub(crate) serial: u64,
}

/// A number no other plugin graph build of this process has.
fn next_build() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Where a plugin graph stands, for Project Graphs to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginMapStatus {
    /// Asked for, and not back yet.
    Building,
    /// The last build gave nothing; why.
    Failed(String),
    /// The plugin answered in its own words instead of a graph.
    Said(String),
    /// Built: drawn as any other graph.
    Built,
}

/// Why the graph picked last in Project Graphs is not among them now, for
/// the note that says so where it stood: it went with no word, and the
/// first graph stood in its place as if picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HiddenGraph {
    /// Its plugin is turned off; on again, the graph is back.
    TurnedOff {
        plugin_id: String,
        plugin: String,
        graph: String,
    },
    /// Its plugin, as installed now, no longer adds it.
    Dropped { plugin: String },
}

/// The map id of plugin `plugin`'s graph `graph`, as Project Graphs keeps
/// it.
pub(crate) fn map_id(plugin: &str, graph: &str) -> String {
    format!("plugin:{plugin}/{graph}")
}

/// The plugin and graph a map id names, if a plugin's graph.
pub(crate) fn map_of(id: &str) -> Option<(&str, &str)> {
    id.strip_prefix("plugin:")?.split_once('/')
}

/// A graph a plugin built, as Project Graphs draws a map.
fn project_map(
    id: String,
    decl: &clew_plugin::manifest::GraphDecl,
    graph: Graph,
) -> clew_protocol::ProjectMap {
    clew_protocol::ProjectMap {
        id,
        title: decl.title_in(crate::i18n::current().code()).to_string(),
        description: decl.description.clone(),
        nodes: graph
            .nodes
            .into_iter()
            .map(|n| clew_protocol::MapNode {
                label: n.label,
                kind: n.kind,
                file: n.at.as_ref().map(|at| at.path.clone()).unwrap_or_default(),
                line: n.at.map_or(0, |at| at.line as usize),
                detail: n.detail,
            })
            .collect(),
        edges: graph
            .edges
            .into_iter()
            .map(|e| clew_protocol::MapEdge {
                from: e.from as usize,
                to: e.to as usize,
                label: e.label,
            })
            .collect(),
    }
}

/// A place in Settings → Plugins that a notice or a link opens at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsSpot {
    /// A plugin's row, by id: its log, and what fixes it.
    Plugin(String),
    /// Building plugins: Rust, and its setup.
    Rust,
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
    /// Asked from the code's menu, at this line.
    pub(crate) at: Option<Spot>,
}

/// Something asked before it is done: what it is, said whole, and what
/// doing it does.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub(crate) title: String,
    /// What it is about, in full: the address a link goes to.
    pub(crate) detail: String,
    /// The button that does it.
    pub(crate) yes: String,
    /// It takes something away.
    pub(crate) destructive: bool,
    pub(crate) then: Then,
}

/// What a [`Confirm`] does when the user says yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Then {
    /// Open a web address a plugin's page links to.
    OpenLink(String),
    /// Remove an installed plugin, by id.
    Remove(String),
    /// Remove the Rust Clew installed for itself.
    RemoveOwnRust,
}

/// What an agent tries of a build: a command, run as if from where the
/// user is, or a graph, built.
pub(crate) struct Trial {
    /// Empty when a graph is tried.
    pub(crate) command: String,
    pub(crate) graph: Option<String>,
    pub(crate) file: Option<String>,
    pub(crate) line: Option<u32>,
    pub(crate) input: Option<String>,
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
    /// The run that showed it, to run again from the page: come back to
    /// from a row opened, it may be stale, and running it again meant the
    /// palette and the question typed anew.
    pub(crate) again: Option<Again>,
}

impl PluginPage {
    /// The page for `view`, its Markdown parsed and its code coloured once,
    /// here, rather than at every frame.
    pub(crate) fn new(id: String, plugin: String, mut view: View) -> Self {
        use clew_plugin::runtime::Block;
        // Clew's own notice that the page was cut, in the reader's language.
        for block in &mut view.blocks {
            if let Block::Notice(n) = block
                && n.text == clew_plugin::runtime::CUT_NOTICE
            {
                n.text =
                    tr!("The plugin gave more than Clew shows on a page; the rest is left out.")
                        .to_string();
            }
        }
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
            again: None,
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
    pub(crate) loaded: HashMap<String, Arc<Loaded>>,
    /// The command running, by its title, as the status bar says it.
    pub(crate) running: Option<String>,
    /// That command as it was run: a page's ↻ turns only for its own run.
    pub(crate) running_again: Option<Again>,
    /// How far it has come, as it says (`plugin_host::unpack_progress`).
    pub(crate) progress: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Set to stop it (`Host::stop_asked`): its chip's Stop.
    pub(crate) stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) install: Questions<PendingInstall>,
    pub(crate) prompt: Option<Prompt>,
    /// What is being asked before it is done.
    pub(crate) confirm: Option<Confirm>,
    /// What each plugin wrote to its log, by id, the latest last.
    pub(crate) log: HashMap<String, VecDeque<(Level, String)>>,
    /// How many of each plugin's earliest log lines were let go to keep it
    /// to [`LOG_LINES`].
    pub(crate) log_dropped: HashMap<String, usize>,
    /// Why `plugins.toml` could not be read, when it could not: the list
    /// kept is the last one read, and Settings says so.
    pub(crate) list_error: Option<String>,
    /// Plugins installed whose manifest this Clew cannot read (a later one
    /// installed them, or their record was changed), by id, with the name
    /// it gives them where that much reads, else the id: not run, and
    /// listed in Settings with what to do — they vanished.
    pub(crate) unreadable: Vec<(String, String)>,
    /// The plugins whose last command run failed, by id: until one
    /// finishes, or a new build is installed, its Settings row says what
    /// fixes it.
    pub(crate) failed: HashSet<String>,
    /// The plugins whose last graph build failed: a graph built well says
    /// nothing of a command that crashed, nor the other way round.
    pub(crate) graph_failed: HashSet<String>,
    /// The plugins whose approved copy changed on disk, by id: not run,
    /// until installed again — nothing an agent's fix or a Try again
    /// changes.
    pub(crate) damaged: HashSet<String>,
    /// The plugins whose whole log Settings shows, by id; the others show
    /// their last lines.
    pub(crate) log_open: HashSet<String>,
    /// Why the last install from a file, or the last change, failed.
    pub(crate) failure: Option<String>,
    /// The requests of `clew-server mcp` this window took on.
    pub(crate) handling: HashSet<String>,
    /// What stops the plugin tools an Ask turn is running (a build of
    /// minutes), by the turn's stream: set when the turn is stopped.
    pub(crate) tool_cancels: HashMap<u64, Arc<AtomicBool>>,
    /// Whether plugins can be built here, as Settings → Plugins says.
    pub(crate) building: Building,
    /// The Rust Clew installed is there, used or not.
    pub(crate) own_rust: bool,
    /// Why the last setup step did not work, until one works or another
    /// starts.
    pub(crate) setup_failure: Option<String>,
    /// Why Clew's own Rust could not be removed, until it is.
    pub(crate) removal_failure: Option<String>,
    /// A build of the Ask agent's failed: once the check says Rust is
    /// what is missing, the status line says so, with Settings.
    pub(crate) say_missing_rust: bool,
    /// Settings opens about this: scrolled to it.
    pub(crate) reveal_in_settings: Option<SettingsSpot>,
}

/// Whether plugins can be built on this computer.
#[derive(Debug, Clone, Default)]
pub(crate) enum Building {
    /// Not looked at yet.
    #[default]
    Unknown,
    Checking,
    /// Ready: the Rust version, and whether it is Clew's own.
    Ready {
        version: String,
        own: bool,
    },
    Missing(clew_plugin::toolchain::Missing),
    /// A setup step running, and how it goes.
    SettingUp {
        stage: clew_plugin::toolchain::Stage,
        cancel: Arc<AtomicBool>,
    },
    /// Clew's own Rust being removed: "Checking…" said nothing of the
    /// 700 MB going.
    Removing,
    /// Another Clew window is setting Rust up or removing it: looked at
    /// again when it is done.
    Elsewhere,
}

/// What a check of the toolchain found.
#[derive(Debug, Clone)]
pub struct Checked {
    pub(crate) found: Result<(String, bool), clew_plugin::toolchain::Missing>,
    /// Clew's own Rust is installed, used or not.
    pub(crate) own_installed: bool,
    /// Asked to remove it: why it is still there.
    pub(crate) removal_failed: Option<String>,
    /// Another Clew window is setting Rust up or removing it: nothing
    /// else was looked at.
    pub(crate) elsewhere: bool,
    /// What removing Clew's own Rust came to — not a check that ended
    /// while it went, which read the removal's own lock as another
    /// window's.
    pub(crate) removal: bool,
}

impl Checked {
    /// Rust as it is now, unless another window is changing it.
    fn now(data: &Path) -> Checked {
        if clew_plugin::toolchain::busy(data) {
            return Checked::elsewhere();
        }
        Checked {
            found: clew_plugin::toolchain::find(Some(data))
                .map(|t| (t.version, t.cargo_home.is_some())),
            own_installed: clew_plugin::toolchain::managed_installed(data),
            removal_failed: None,
            elsewhere: false,
            removal: false,
        }
    }

    fn elsewhere() -> Checked {
        Checked {
            found: Err(clew_plugin::toolchain::Missing {
                why: clew_plugin::toolchain::Why::Other(clew_plugin::toolchain::ELSEWHERE.into()),
                setup: None,
                rustup: false,
            }),
            own_installed: false,
            removal_failed: None,
            elsewhere: true,
            removal: false,
        }
    }

    /// The check's thread ended without a word.
    fn failed() -> Checked {
        Checked {
            found: Err(clew_plugin::toolchain::Missing {
                why: clew_plugin::toolchain::Why::Other(
                    tr!("the check failed unexpectedly").into(),
                ),
                setup: None,
                rustup: false,
            }),
            own_installed: false,
            removal_failed: None,
            elsewhere: false,
            removal: false,
        }
    }
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
        let (installed, unreadable, list_error) = match Self::read_installed() {
            Ok((installed, unreadable)) => (installed, unreadable, None),
            Err(why) => (Vec::new(), Vec::new(), Some(why)),
        };
        Plugins {
            installed,
            unreadable,
            list_error,
            ..Default::default()
        }
    }

    /// The plugins installed, as `plugins.toml` lists them, and those whose
    /// manifest cannot be read, by id with why; why not, when the list is
    /// there and cannot be read — never taken for no plugins.
    #[allow(clippy::type_complexity)]
    pub(crate) fn read_installed() -> Result<(Vec<Installed>, Vec<(String, String)>), String> {
        let Some(data) = clew_core::lsp::store::data_root() else {
            return Ok((Vec::new(), Vec::new()));
        };
        let store = Store::load_checked(&data)?;
        let mut installed = Vec::new();
        let mut unreadable = Vec::new();
        for (id, record) in store.plugin {
            let manifest = match clew_plugin::manifest::Manifest::parse(&record.manifest) {
                Ok(manifest) => manifest,
                Err(why) => {
                    // Why, for whoever reads the log: the row says what
                    // the reader can do, not the parser's words.
                    // Once a process: every window reads the list again
                    // at every change of it.
                    static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                    let line = format!("[clew] plugin {id} is not run, its manifest unread: {why}");
                    let mut said = SAID
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !said.contains(&line) {
                        eprintln!("{line}");
                        said.push(line);
                    }
                    let name = clew_plugin::manifest::Manifest::name_of(&record.manifest)
                        .unwrap_or_else(|| id.clone());
                    unreadable.push((id, name));
                    continue;
                }
            };
            let has_source = clew_plugin::authoring::source_dir(&data, &id)
                .is_ok_and(|dir| dir.join("plugin.toml").is_file());
            installed.push(Installed {
                id,
                record,
                manifest,
                has_source,
            });
        }
        installed.sort_by_key(|p| p.name().to_lowercase());
        unreadable.sort_by_key(|(_, name)| name.to_lowercase());
        Ok((installed, unreadable))
    }

    /// The installed plugin `id`.
    pub(crate) fn get(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|p| p.id == id)
    }

    /// Plugin `id`'s log, a line each, the latest last: empty when it
    /// wrote nothing.
    pub(crate) fn log_text(&self, id: &str) -> String {
        let Some(log) = self.log.get(id).filter(|l| !l.is_empty()) else {
            return String::new();
        };
        let lines = log
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Copied for an agent, which should know it is not the whole log.
        match self.log_dropped.get(id) {
            Some(1) => format!("(1 earlier line was not kept)\n{lines}"),
            Some(n) => format!("({n} earlier lines were not kept)\n{lines}"),
            None => lines,
        }
    }

    /// Whether plugin `id`'s last run or graph build failed: it is not
    /// working, and its row says what fixes it.
    pub(crate) fn failing(&self, id: &str) -> bool {
        self.failed.contains(id) || self.graph_failed.contains(id)
    }

    /// Note `line` in plugin `id`'s log.
    fn note(&mut self, id: &str, level: Level, line: String) {
        let log = self.log.entry(id.to_string()).or_default();
        // A line each: what a plugin printed came as one block, and its
        // "last five lines" were one entry of up to 64 KiB.
        for line in line.lines().filter(|l| !l.trim().is_empty()) {
            log.push_back((level, line.to_string()));
        }
        let mut dropped = 0;
        while log.len() > LOG_LINES {
            log.pop_front();
            dropped += 1;
        }
        if dropped > 0 {
            *self.log_dropped.entry(id.to_string()).or_default() += dropped;
        }
    }

    /// Forget what this window kept of plugin `id`'s copy: replaced or
    /// removed, its log, failure and damaged copy are not the next one's.
    fn forget_copy(&mut self, id: &str) {
        self.log.remove(id);
        self.log_dropped.remove(id);
        self.log_open.remove(id);
        self.failed.remove(id);
        self.graph_failed.remove(id);
        self.damaged.remove(id);
    }
}

/// What came of a run.
pub struct Ran {
    pub(crate) outcome: Result<Outcome, Failure>,
    /// The plugin, ready to run again, by its fingerprint.
    pub(crate) loaded: Option<(String, Arc<Loaded>)>,
    pub(crate) log: Vec<HostEvent>,
}

/// What came of a graph's build.
pub struct GraphRan {
    pub(crate) outcome: Result<Graph, Failure>,
    /// The plugin, ready to run again, by its fingerprint.
    pub(crate) loaded: Option<(String, Arc<Loaded>)>,
    pub(crate) log: Vec<HostEvent>,
}

impl std::fmt::Debug for GraphRan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphRan")
            .field("outcome", &self.outcome.as_ref().map(|g| g.nodes.len()))
            .field("loaded", &self.loaded.as_ref().map(|(fp, _)| fp))
            .field("log", &self.log.len())
            .finish()
    }
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
    /// Run it from the code's right-click menu, at the line clicked.
    RunFromMenu {
        id: String,
        command: String,
    },
    /// Run a command again as it was run: a failed run's Try again.
    RunAgain(Again),
    /// The text typed for a command that asks for some.
    PromptEdit(String),
    /// Run the command with what was typed.
    PromptRun,
    PromptCancel,
    /// While a command runs: look at how far it has come.
    ProgressTick,
    /// A run ended.
    Ran {
        stamp: Stamp,
        id: String,
        title: String,
        again: Again,
        /// The copy of the plugin it ran.
        fingerprint: String,
        ran: Arc<std::sync::Mutex<Option<Ran>>>,
    },
    /// Open the place of row `row` of block `block` of the page shown.
    Open {
        block: usize,
        row: usize,
    },
    /// Put the page away.
    ClosePage,
    /// Show again the page put away by opening one of its rows.
    BackToPage,
    /// Settings: install a plugin from a file.
    InstallFromFile,
    Picked(Option<PathBuf>),
    /// A file read for installing, and where from.
    Read {
        result: Arc<Result<Prepared, String>>,
        origin: Source,
        reply: Reply,
    },
    /// A request's build, read off the window for it.
    RequestRead {
        nonce: String,
        request: Arc<clew_plugin::requests::Request>,
        built: Arc<std::sync::Mutex<Option<Result<clew_plugin::build::Built, String>>>>,
    },
    InstallAllowed(u64),
    InstallDismissed(u64),
    /// A link on a plugin's page: a place in the project opens; a web
    /// address is shown whole and asked about first.
    Link(String),
    /// Settings: copy plugin `id`'s log, to give to whoever fixes it.
    CopyLog(String),
    /// Settings: show plugin `id`'s whole log, or its last lines again.
    ToggleLog(String),
    /// Settings: put a request to fix plugin `id`, with its log, in Ask's
    /// field, and open Ask.
    AskToFix(String),
    /// Stop the command running (its chip's ×).
    StopRun,
    /// Open Settings → Plugins at a plugin's row (its log, and what fixes
    /// it), or at Rust's setup.
    ShowInSettings(SettingsSpot),
    /// Settings: Remove, asked about first.
    AskRemove(String),
    AskRemoveOwnRust,
    /// The question asked first: yes, or not.
    Confirmed,
    ConfirmDismissed,
    /// An install written, or why not.
    Installed {
        id: String,
        reply: Reply,
        /// It replaced another build of it, of this version: an update,
        /// said so.
        update: Option<String>,
        /// It replaced a plugin of its id this Clew could not read: said
        /// so, as the card that asked said Replace.
        replaced: bool,
        result: Result<String, String>,
    },
    SetEnabled(String, bool),
    Remove(String),
    /// Another window changed what is installed or turned on: read the
    /// list again (`STORE_REVISION`).
    StoreChanged,
    /// Another window set Rust up or removed Clew's own: looked at again.
    ToolchainChanged,
    /// Requests `clew-server mcp` filed, as [`requests_subscription`] found
    /// them.
    Requests(Arc<Vec<(String, clew_plugin::requests::Request)>>),
    /// The requests taken on changed: one answered, or withdrawn by its
    /// agent, who stopped waiting (`App::drop_withdrawn_installs`).
    TakenChanged,
    /// An Ask agent's plugin tool call has its answer.
    ToolAnswered {
        stream: u64,
        call: u64,
        /// The tool called, for its step's chip.
        tool: String,
        /// The plugin it was called for (its `id` argument), if any.
        plugin: Option<String>,
        answer: Arc<std::sync::Mutex<Option<clew_plugin::tools::Step>>>,
    },
    /// A try for an agent ran: the answer for the agent.
    Tried {
        reply: Reply,
        answer: Result<String, String>,
    },
    /// A plugin graph's build ended: the map, the files and the copy of
    /// the plugin it was asked for.
    GraphBuilt {
        stamp: Stamp,
        map: String,
        rev: u64,
        fingerprint: String,
        /// Which build it was ([`PluginMap::serial`]).
        serial: u64,
        ran: Arc<std::sync::Mutex<Option<GraphRan>>>,
    },
    /// Build the plugin graph shown again, whatever came of the last build.
    RebuildGraph,
    /// Project Graphs: the note on a graph no longer offered, read; the
    /// graph shown in its place is the one kept.
    ForgetHiddenGraph,
    /// Settings → Plugins: whether plugins can be built here was checked.
    ToolchainChecked(Checked),
    /// Settings: look again whether plugins can be built — after the user
    /// set Rust up outside Clew.
    CheckToolchain,
    /// Take the step that makes plugin building ready.
    SetUpToolchain,
    /// How the step goes.
    ToolchainStage(clew_plugin::toolchain::Stage),
    /// The step ended: ready with this Rust, or why not.
    /// A setup step ended: the Rust version now ready, or why not (`None`:
    /// the user cancelled it).
    ToolchainSetUp(Result<String, Option<clew_plugin::toolchain::SetUpError>>),
    CancelToolchainSetup,
    /// Remove the Rust Clew installed.
    RemoveOwnRust,
}

impl PluginMsg {
    /// Where the message comes from, for `dispatch`'s ownership check.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            PluginMsg::Ran { stamp, .. } | PluginMsg::GraphBuilt { stamp, .. } => {
                Some(Origin::Stamped(stamp))
            }
            _ => None,
        }
    }
}

/// Bumped whenever this process changes what is installed or turned on: a
/// window read the list as it opened, and went on offering — and running,
/// from its own copy — a plugin another window had turned off, removed or
/// replaced. The shell tells the others (`PluginMsg::StoreChanged`).
static STORE_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The plugins' store's revision (`STORE_REVISION`).
pub(crate) fn store_revision() -> u64 {
    STORE_REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

fn store_changed() {
    STORE_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Bumped when this process ends setting Rust up or removing Clew's own:
/// another window's Settings, read before, offered to install it again —
/// 700 MB for nothing. The shell tells the others
/// (`PluginMsg::ToolchainChanged`).
static TOOLCHAIN_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn toolchain_revision() -> u64 {
    TOOLCHAIN_REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

fn toolchain_changed() {
    TOOLCHAIN_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Why a run of `record` stopped before it started because its approval
/// no longer stands as this window read it.
const WITHDRAWN: [&str; 4] = [
    tr_noop!("it was removed"),
    tr_noop!("it was updated"),
    tr_noop!("it is turned off"),
    tr_noop!("Clew could not read its list of plugins"),
];

/// Why `id`'s plugin, as `record` has it, may no longer run: turned off,
/// removed or replaced since this window read the list — by another copy
/// of Clew, which no message reaches. Asked as each run starts, of the
/// copy this window keeps loaded too.
fn withdrawn(data: &std::path::Path, id: &str, record: &Record) -> Option<&'static str> {
    // Unreadable is not "removed": nothing is taken from what it saved.
    let Ok(store) = Store::load_checked(data) else {
        return Some(WITHDRAWN[3]);
    };
    match store.plugin.get(id) {
        None => Some(WITHDRAWN[0]),
        Some(now) if now.fingerprint != record.fingerprint || now.sha256 != record.sha256 => {
            Some(WITHDRAWN[1])
        }
        Some(now) if !now.enabled => Some(WITHDRAWN[2]),
        Some(_) => None,
    }
}

/// Whether a run or build that ended with `outcome` read the list of
/// plugins this window could not: past the check every run starts with
/// (`withdrawn`), it reads now. Said of it, and noted in Settings, stood on
/// after a run that worked.
fn list_read_again<T>(plugins: &Plugins, outcome: &Result<T, Failure>) -> bool {
    plugins.list_error.is_some()
        && !matches!(outcome, Err(Failure::Stopped(why)) if why == WITHDRAWN[3])
}

/// Whether `failure` is a run refused because its approval no longer
/// stands (`withdrawn`): nothing the plugin did.
fn is_withdrawn(failure: &Failure) -> bool {
    matches!(failure, Failure::Stopped(why) if WITHDRAWN.contains(&why.as_str()))
}

/// What a plugin tool's step says as it runs, waiting at its card, done
/// and failed: each in words of its own. A failure was the running words
/// with their "…" cut — "Building a plugin" beside a cross, and in
/// Chinese, Japanese and Korean still "building".
fn plugin_step_title(tool: &str, state: crate::app::model::StepState) -> String {
    use crate::app::model::StepState::{Done, Failed, Waiting};
    match (tool, state) {
        ("plugin_guide", Done) => tr!("Read the plugin guide"),
        ("plugin_guide", Failed) => tr!("Could not read the plugin guide"),
        ("plugin_guide", _) => tr!("Reading the plugin guide…"),
        ("plugin_new", Done) => tr!("Started a plugin"),
        ("plugin_new", Failed) => tr!("Could not start a plugin"),
        ("plugin_new", _) => tr!("Starting a plugin…"),
        ("plugin_files" | "plugin_read", Done) => tr!("Read a plugin's source"),
        ("plugin_files" | "plugin_read", Failed) => tr!("Could not read a plugin's source"),
        ("plugin_files" | "plugin_read", _) => tr!("Reading a plugin's source…"),
        ("plugin_write", Done) => tr!("Wrote a plugin's source"),
        ("plugin_write", Failed) => tr!("Could not write a plugin's source"),
        ("plugin_write", _) => tr!("Writing a plugin's source…"),
        ("plugin_build", Done) => tr!("Built a plugin"),
        ("plugin_build", Failed) => tr!("The plugin did not build"),
        ("plugin_build", _) => tr!("Building a plugin…"),
        ("plugin_try", Done) => tr!("Tried a plugin"),
        ("plugin_try", Failed) => tr!("Could not try the plugin"),
        ("plugin_try", _) => tr!("Trying a plugin…"),
        ("plugin_install", Done) => tr!("Installed a plugin"),
        ("plugin_install", Failed) => tr!("The plugin was not installed"),
        ("plugin_install", Waiting) => tr!("Asking you to install a plugin"),
        ("plugin_install", _) => tr!("Asking to install a plugin…"),
        (_, Done) => tr!("Looked at the plugins"),
        (_, Failed) => tr!("Could not look at the plugins"),
        (_, _) => tr!("Looking at the plugins…"),
    }
    .to_string()
}

/// The plugin tools whose steps Ask shows (`plugin_step_title`).
const PLUGIN_STEP_TOOLS: [&str; 8] = [
    "plugin_guide",
    "plugin_new",
    "plugin_read",
    "plugin_write",
    "plugin_build",
    "plugin_try",
    "plugin_install",
    "plugin_status",
];

/// A plugin step its turn ended before it was answered, said as failed in
/// its own words (`plugin_step_title`), not as one still going.
pub(crate) fn plugin_step_cut_short(title: &str) -> String {
    use crate::app::model::StepState::{Failed, Running, Waiting};
    PLUGIN_STEP_TOOLS
        .iter()
        .find(|tool| {
            title == plugin_step_title(tool, Running) || title == plugin_step_title(tool, Waiting)
        })
        .map(|tool| plugin_step_title(tool, Failed))
        // Said in another language since: as it was, without its "…".
        .unwrap_or_else(|| title.trim_end_matches('…').to_string())
}

/// Why setting Rust up did not work, in the reader's words: why plugins
/// still cannot be built, or Clew's own reason; rustup's own words as it
/// wrote them. It was the English sentence written for agents, inside a
/// translated one.
fn setup_error_words(why: &clew_plugin::toolchain::SetUpError) -> String {
    match why {
        clew_plugin::toolchain::SetUpError::Missing(missing) => {
            crate::ui::missing_words(&missing.why)
        }
        clew_plugin::toolchain::SetUpError::Failed(why) => crate::i18n::lookup(why).to_string(),
        clew_plugin::toolchain::SetUpError::Elsewhere => {
            crate::i18n::lookup(clew_plugin::toolchain::ELSEWHERE).to_string()
        }
    }
}

/// Delete what plugin `id` kept between runs (`clew_plugin::runtime::store_dir`).
fn forget_store_of(data: &Path, id: &str) {
    if let Ok(dir) = clew_plugin::runtime::store_dir(data, id) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Plugin `id` ended a run or a build after it was removed: what it saved
/// meanwhile goes, unless it is installed again since.
fn removed_store_of(id: &str) {
    let Ok(data) = data() else {
        return;
    };
    // Only when the list is read and does not name it: an unreadable list
    // deleted the saved data of a plugin still installed.
    if Store::load_checked(&data).is_ok_and(|store| !store.plugin.contains_key(id)) {
        forget_store_of(&data, id);
    }
}

/// A plugin's file picked to install, read only as far as a plugin may be
/// long, and only if it is a plain file: a multi-gigabyte file picked by
/// mistake was read whole before its size was looked at, and a pipe held a
/// thread for good.
fn read_plugin_file(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let cap = clew_plugin::inspect::MAX_PLUGIN_BYTES;
    let file = match clew_core::statefile::open_plain_checked(path) {
        Ok(Some(file)) => file,
        Ok(None) => return Err(tr!("the file is not there").into()),
        Err(clew_core::statefile::ReadError::NotPlainFile) => {
            return Err(tr!("it is not a plain file").into());
        }
        Err(e) => return Err(e.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > cap {
        return Err(tr!(
            "it is larger than a plugin may be ({size} MB at most)",
            size = cap / (1024 * 1024)
        ));
    }
    Ok(bytes)
}

/// The last part of `path`, as a file picked is named to the reader.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A reason clew-plugin wrote, in the reader's words: one of its sentences
/// whole, or one followed by ": " and what went wrong — the sentence
/// translated, what follows (a permission's name, wasmtime's own words) as
/// it was written.
pub(crate) fn reason_words(why: &str) -> String {
    use clew_plugin::{runtime, store};
    let detail = |sentence: &str| {
        why.strip_prefix(sentence)
            .and_then(|rest| rest.strip_prefix(": "))
            .map(str::to_string)
    };
    if let Some(reason) = detail(runtime::NOT_GRANTED) {
        return tr!(
            "the plugin needs a permission it was not granted: {reason}",
            reason = reason
        );
    }
    if let Some(reason) = detail(runtime::NOT_COMPILED) {
        return tr!("could not compile the plugin: {reason}", reason = reason);
    }
    if let Some(reason) = detail(runtime::NOT_LINKED) {
        return tr!("could not link the plugin: {reason}", reason = reason);
    }
    if let Some(reason) = detail(runtime::NO_EXPORTS) {
        return tr!(
            "the plugin does not export what Clew calls: {reason}",
            reason = reason
        );
    }
    if let Some(reason) = detail(store::NOT_KEPT) {
        return tr!("could not keep the plugin: {reason}", reason = reason);
    }
    crate::i18n::lookup(why).to_string()
}

/// What Clew adds to a plugin's log of its own, in the reader's words.
pub(crate) fn log_note_words(note: clew_plugin::runtime::LogNote) -> String {
    use clew_plugin::runtime::LogNote;
    match note {
        LogNote::OutputCut => tr!(
            "what it printed past {kib} KiB was left out",
            kib = LogNote::OUTPUT_KIB
        ),
        LogNote::LinesDropped(dropped) => trn!(
            "1 more log line was left out: a run keeps {max} lines at most",
            "{n} more log lines were left out: a run keeps {max} lines at most",
            dropped,
            max = crate::i18n::number(LogNote::LINES_KEPT as u64)
        ),
    }
}

/// Why a file picked to install is not one Clew installs, in a sentence.
fn not_installable(why: &str) -> String {
    if why == clew_plugin::inspect::NOT_A_COMPONENT {
        // Its own words already say "not a plugin".
        return tr!(
            "That file is not a plugin: a plugin is a WebAssembly component, built for wasm32-wasip2."
        )
        .into();
    }
    tr!(
        "That file is not a plugin Clew can install: {reason}",
        reason = crate::i18n::lookup(why).to_string()
    )
}

/// The most of a try's answer an agent is handed: well under what
/// `clew_plugin::requests` reads back for `clew-server mcp`.
pub(crate) const TRY_ANSWER_MAX: usize = 256 * 1024;

/// `answer`, cut to [`TRY_ANSWER_MAX`] at a character's edge, saying so.
pub(crate) fn cut_answer(mut answer: String) -> String {
    if answer.len() <= TRY_ANSWER_MAX {
        return answer;
    }
    let mut at = TRY_ANSWER_MAX;
    while !answer.is_char_boundary(at) {
        at -= 1;
    }
    answer.truncate(at);
    answer.push_str("\n\n(The rest was cut: the answer was too long.)");
    answer
}

/// The data folder, or why there is none.
fn data() -> Result<PathBuf, String> {
    clew_core::lsp::store::data_root().ok_or_else(|| tr!("Clew has no data folder").to_string())
}

/// A failure, in the reader's words. What a plugin says of itself is shown
/// as it wrote it; why Clew stopped it is Clew's to say.
fn failure_words(failure: &Failure) -> String {
    match failure {
        Failure::Plugin(why) => why.clone(),
        Failure::TimedOut(secs) => tr!(
            "it took longer than {seconds} s and was stopped",
            seconds = secs.to_string()
        ),
        Failure::OutOfMemory(mb) => tr!(
            "it needed more than {megabytes} MB of memory and was stopped",
            megabytes = mb.to_string()
        ),
        Failure::Crashed(why) => tr!("it crashed: {reason}", reason = why.clone()),
        Failure::Stopped(why) => reason_words(why),
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
    pub(crate) fn reload_plugins(&mut self) -> Task<Message> {
        match Plugins::read_installed() {
            Ok((installed, unreadable)) => {
                self.plugins.unreadable = unreadable;
                // A plugin replaced or removed (here or in another window):
                // what this window kept of the copy it had — its log, its
                // failure, its damaged copy — is not the new copy's.
                // Installed again counts too, the same build included (its
                // install time is new): installed again because its copy
                // was damaged, "install it again" stood on in every window
                // but the one that did it.
                let now: HashMap<&str, (&str, u64)> = installed
                    .iter()
                    .map(|p| {
                        (
                            p.id.as_str(),
                            (p.record.fingerprint.as_str(), p.record.installed),
                        )
                    })
                    .collect();
                let gone: Vec<(String, bool)> = self
                    .plugins
                    .installed
                    .iter()
                    .filter_map(|p| match now.get(p.id.as_str()) {
                        None => Some((p.id.clone(), true)),
                        Some(&(fingerprint, at)) => (fingerprint != p.record.fingerprint
                            || at != p.record.installed)
                            .then(|| (p.id.clone(), false)),
                    })
                    .collect();
                for (id, removed) in &gone {
                    self.forget_copy_of(id, *removed);
                }
                self.plugins.installed = installed;
                // Readable again: what was said of it goes, and graphs it
                // kept from building are built.
                if self.plugins.list_error.take().is_some() {
                    let said_of_list = self.failure_notice.as_ref().is_some_and(|f| {
                        f.of == FailureOf::PluginRun && f.retry.is_none() && f.spot.is_none()
                    });
                    if said_of_list {
                        self.forget_failure_of(FailureOf::PluginRun);
                    }
                    for map in self.proj.plugin_maps.values_mut().filter(|m| !m.building) {
                        map.rev = u64::MAX;
                    }
                }
            }
            // Unreadable: the list read last stands, and Settings says why.
            Err(why) => self.plugins.list_error = Some(why),
        }
        // A card waiting to install a plugin turned off, removed or
        // replaced meanwhile (here or in another window) said "Update",
        // "Newly asked" or "This turns it on" of what was there before:
        // asked again, of what is there now.
        let installed = self.plugins.installed.clone();
        let unreadable = self.plugins.unreadable.clone();
        self.plugins.install.renew(|pending| {
            let id = &pending.inspected.manifest.plugin.id;
            let now = installed.iter().find(|p| p.id == *id).cloned();
            let unreadable = unreadable
                .iter()
                .find(|(at, _)| at == id)
                .map(|(_, name)| name.clone());
            let same = unreadable == pending.unreadable
                && match (&now, &pending.previous) {
                    (None, None) => true,
                    (Some(a), Some(b)) => {
                        a.record.fingerprint == b.record.fingerprint
                            && a.record.enabled == b.record.enabled
                    }
                    _ => false,
                };
            (!same).then(|| PendingInstall {
                previous: now,
                unreadable,
                ..pending.clone()
            })
        });
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
        // The graphs offered follow the plugins: one turned off or removed
        // takes its own away, one installed brings its own.
        let shown = |app: &App| {
            app.selected_map_index()
                .and_then(|at| app.proj.maps.get(at))
                .cloned()
        };
        let open = self.proj.overlay == Some(Overlay::ProjectMaps);
        let before = open.then(|| shown(self));
        let composed = self.compose_maps();
        // Drawn as it is now: the graph of a plugin turned off stood drawn
        // under the title of the one that took its place, and a plugin's
        // new copy never built the graph it shows.
        let relayout = match open && Some(shown(self)) != before {
            true => {
                self.proj.graph_pick = None;
                self.refresh_graph_layout()
            }
            false => Task::none(),
        };
        Task::batch([composed, relayout, self.ensure_plugin_map()])
    }

    /// The plugin graph picked last in Project Graphs, if it is not offered
    /// now, and why. `None` while it is offered, or before the project's
    /// graphs are read, or when its plugin was removed: removing said its
    /// graphs go.
    pub(crate) fn hidden_plugin_graph(&self) -> Option<HiddenGraph> {
        let id = self.proj.maps_selected.as_deref()?;
        if self.proj.maps.iter().any(|m| m.id == id) {
            return None;
        }
        let (plugin, graph) = map_of(id)?;
        let installed = self.plugins.get(plugin)?;
        match installed.manifest.graph(graph) {
            None => Some(HiddenGraph::Dropped {
                plugin: installed.name(),
            }),
            Some(decl) if !installed.record.enabled => Some(HiddenGraph::TurnedOff {
                plugin_id: installed.id.clone(),
                plugin: installed.name(),
                graph: decl.title_in(crate::i18n::current().code()).to_string(),
            }),
            // On, and adding it: offered once the project's are read.
            Some(_) => None,
        }
    }

    /// The graphs the plugins on now add: each plugin with its declaration.
    fn plugin_graphs(&self) -> Vec<(&Installed, &clew_plugin::manifest::GraphDecl)> {
        self.plugins
            .installed
            .iter()
            .filter(|p| p.record.enabled)
            .flat_map(|p| p.manifest.graphs.iter().map(move |g| (p, g)))
            .collect()
    }

    /// Project Graphs' list: the project's own maps, then — once they were
    /// read, so the first shown is the project's — the plugins' graphs, as
    /// built by the copy installed now, or empty until they are.
    pub(crate) fn compose_maps(&mut self) -> Task<Message> {
        let mut maps = (*self.proj.core_maps).clone();
        let read = self.proj.maps_rev.is_some() || self.proj.maps_error.is_some();
        if read {
            for (plugin, decl) in self.plugin_graphs() {
                let id = map_id(&plugin.id, &decl.id);
                let built = self
                    .proj
                    .plugin_maps
                    .get(&id)
                    .filter(|m| m.fingerprint == plugin.record.fingerprint)
                    .and_then(|m| m.built.clone());
                maps.push(built.unwrap_or_else(|| clew_protocol::ProjectMap {
                    id,
                    title: decl.title_in(crate::i18n::current().code()).to_string(),
                    description: decl.description.clone(),
                    nodes: Vec::new(),
                    edges: Vec::new(),
                }));
            }
        }
        let retired = std::mem::replace(&mut self.proj.maps, Arc::new(maps));
        crate::app::graph::free_off_thread(retired)
    }

    /// The plugin graph Project Graphs shows, and its plugin, if the map
    /// shown is a plugin's.
    fn shown_plugin_graph(
        &self,
    ) -> Option<(String, &Installed, &clew_plugin::manifest::GraphDecl)> {
        let at = self.selected_map_index()?;
        let id = self.proj.maps.get(at)?.id.clone();
        let (plugin, graph) = map_of(&id)?;
        let installed = self.plugins.get(plugin).filter(|p| p.record.enabled)?;
        let decl = installed.manifest.graph(graph)?;
        Some((id, installed, decl))
    }

    /// Where the plugin graph shown stands; `None` for a map of the
    /// project's own.
    pub(crate) fn plugin_map_status(&self) -> Option<PluginMapStatus> {
        let (id, plugin, _) = self.shown_plugin_graph()?;
        let entry = self
            .proj
            .plugin_maps
            .get(&id)
            .filter(|m| m.fingerprint == plugin.record.fingerprint);
        Some(match entry {
            Some(m) if m.building => PluginMapStatus::Building,
            Some(PluginMap {
                failed: Some(why), ..
            }) => PluginMapStatus::Failed(why.clone()),
            Some(PluginMap {
                said: Some(said), ..
            }) => PluginMapStatus::Said(said.clone()),
            Some(PluginMap { built: Some(_), .. }) => PluginMapStatus::Built,
            // Not asked yet: it is, as soon as it is shown.
            _ => PluginMapStatus::Building,
        })
    }

    /// The command running, how far it has come in percent when it says,
    /// and whether it was asked to stop: for its chip.
    pub(crate) fn plugin_run(&self) -> Option<(&str, Option<u32>, bool)> {
        let title = self.plugins.running.as_deref()?;
        let percent = self.plugins.progress.as_ref().and_then(|p| {
            let (done, total) = crate::app::plugin_host::unpack_progress(
                p.load(std::sync::atomic::Ordering::Relaxed),
            );
            (total > 0).then(|| (u64::from(done.min(total)) * 100 / u64::from(total)) as u32)
        });
        let stopping = self
            .plugins
            .stop
            .as_ref()
            .is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed));
        Some((title, percent, stopping))
    }

    /// The plugin that adds the graph shown, by its id.
    pub(crate) fn shown_graph_plugin_id(&self) -> Option<String> {
        self.shown_plugin_graph().map(|(_, p, _)| p.id.clone())
    }

    /// The plugin that adds the graph shown, by its name.
    pub(crate) fn shown_graph_plugin(&self) -> Option<String> {
        self.shown_plugin_graph().map(|(_, p, _)| p.name())
    }

    /// Look again at whether plugins can be built here, unless a setup
    /// step is running (it looks when it ends).
    pub(crate) fn check_toolchain(&mut self) -> Task<Message> {
        // Waiting on another window already looks again when it is done.
        if matches!(
            self.plugins.building,
            Building::Checking
                | Building::SettingUp { .. }
                | Building::Removing
                | Building::Elsewhere
        ) {
            return Task::none();
        }
        let Ok(data) = data() else {
            return Task::none();
        };
        self.plugins.building = Building::Checking;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || Checked::now(&data))
                    .await
                    .unwrap_or_else(|_| Checked::failed())
            },
            |checked| Message::Plugin(PluginMsg::ToolchainChecked(checked)),
        )
    }

    /// Another window is setting Rust up or removing it: look again when
    /// it is done, with nothing to press meanwhile.
    fn check_toolchain_when_free(&mut self) -> Task<Message> {
        let Ok(data) = data() else {
            return Task::none();
        };
        self.plugins.building = Building::Elsewhere;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    clew_plugin::toolchain::wait_while_busy(&data);
                    Checked::now(&data)
                })
                .await
                .unwrap_or_else(|_| Checked::failed())
            },
            |checked| Message::Plugin(PluginMsg::ToolchainChecked(checked)),
        )
    }

    /// Take the step the check offered, telling Settings how it goes.
    fn set_up_toolchain(&mut self) -> Task<Message> {
        use iced::futures::channel::mpsc;
        let setup = match &self.plugins.building {
            Building::Missing(missing) => missing.setup.clone(),
            _ => None,
        };
        let (Some(setup), Ok(data)) = (setup, data()) else {
            return Task::none();
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let first = match setup {
            clew_plugin::toolchain::Setup::AddTarget { .. } => {
                clew_plugin::toolchain::Stage::AddingTarget
            }
            clew_plugin::toolchain::Setup::Install => clew_plugin::toolchain::Stage::Downloading,
        };
        self.plugins.setup_failure = None;
        self.plugins.building = Building::SettingUp {
            stage: first,
            cancel: cancel.clone(),
        };
        let (tx, rx) = mpsc::unbounded::<Message>();
        std::thread::spawn(move || {
            let stages = tx.clone();
            let mut stage = move |stage| {
                let _ = stages.unbounded_send(Message::Plugin(PluginMsg::ToolchainStage(stage)));
            };
            let done = clew_plugin::toolchain::set_up(&data, &setup, &cancel, &mut stage)
                .map(|toolchain| toolchain.version)
                .map_err(|why| (!cancel.load(std::sync::atomic::Ordering::Relaxed)).then_some(why));
            let _ = tx.unbounded_send(Message::Plugin(PluginMsg::ToolchainSetUp(done)));
        });
        Task::run(rx, std::convert::identity)
    }

    /// Stop setting Rust up, as its Cancel does — closing the window too.
    /// Adding the target to the user's own Rust is not stopped: it runs to
    /// its end ([`clew_plugin::toolchain::set_up`] does not stop it either),
    /// and a failure of it is said, not taken for a cancel.
    pub(crate) fn cancel_toolchain_setup(&self) {
        if let Building::SettingUp { cancel, stage } = &self.plugins.building
            && *stage != clew_plugin::toolchain::Stage::AddingTarget
        {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Plugin `id`'s commands by the names the palette gives them now, in
    /// the order of its `[[command]]`s; none while it is not offered (off,
    /// or not installed).
    pub(crate) fn palette_names_of(&self, id: &str) -> Vec<String> {
        let Some(plugin) = self.plugins.get(id) else {
            return Vec::new();
        };
        let palette = self.palette_commands();
        let named: Vec<String> = plugin
            .manifest
            .commands
            .iter()
            .filter_map(|command| {
                palette.iter().find_map(|c| match &c.action {
                    crate::app::palette::PaletteAction::Send(Message::Plugin(PluginMsg::Run {
                        id: of,
                        command: which,
                    })) if of == id && *which == command.id => Some(c.title.clone()),
                    _ => None,
                })
            })
            .collect();
        match named.len() == plugin.manifest.commands.len() {
            true => named,
            false => Vec::new(),
        }
    }

    /// Build the plugin graph Project Graphs shows if what it was built
    /// from moved: the project's files, or the copy of the plugin. Never
    /// twice at once.
    pub(crate) fn ensure_plugin_map(&mut self) -> Task<Message> {
        if self.proj.overlay != Some(Overlay::ProjectMaps) {
            self.stop_plugin_map_builds(None);
            return Task::none();
        }
        let Some((id, _, _)) = self.shown_plugin_graph() else {
            self.stop_plugin_map_builds(None);
            return Task::none();
        };
        // Only the graph shown is built: one picked before it stops.
        self.stop_plugin_map_builds(Some(&id));
        let Some((id, plugin, decl)) = self.shown_plugin_graph() else {
            return Task::none();
        };
        let rev = self.proj.registry.revision();
        let fingerprint = plugin.record.fingerprint.clone();
        if let Some(m) = self.proj.plugin_maps.get(&id)
            && m.fingerprint == fingerprint
            && (m.building || m.rev == rev)
        {
            return Task::none();
        }
        let graph = decl.id.clone();
        let record = plugin.record.clone();
        let record_id = plugin.id.clone();
        let entry = self.proj.plugin_maps.entry(id.clone()).or_default();
        if entry.fingerprint != fingerprint {
            // Another copy of the plugin: what the last one built is not
            // this one's, and a build of the last one still going stops.
            if let Some(stop) = entry.stop.take() {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            *entry = PluginMap {
                fingerprint: fingerprint.clone(),
                ..Default::default()
            };
        }
        entry.rev = rev;
        let prepared = match (self.project_host(), self.plugin_runtime(), data()) {
            (Some(host), Ok(runtime), Ok(data)) => Ok((host, runtime, data)),
            (None, ..) => Err(tr!("open a project to build it").to_string()),
            (_, Err(why), _) | (_, _, Err(why)) => Err(why),
        };
        let entry = self.proj.plugin_maps.get_mut(&id).expect("just made");
        let (host, runtime, data) = match prepared {
            Ok(prepared) => prepared,
            Err(why) => {
                entry.failed = Some(why);
                return Task::none();
            }
        };
        entry.building = true;
        let stop = Arc::new(AtomicBool::new(false));
        entry.stop = Some(stop.clone());
        let serial = next_build();
        entry.serial = serial;
        let loaded = self.plugins.loaded.get(&fingerprint).cloned();
        let stamp = self.stamp();
        let done_fp = fingerprint.clone();
        Task::perform(
            async move {
                let handle = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || {
                    if let Some(why) = withdrawn(&data, &record_id, &record) {
                        return GraphRan {
                            outcome: Err(Failure::Stopped(why.to_string())),
                            loaded: None,
                            log: Vec::new(),
                        };
                    }
                    let host = Arc::new(host(handle).with_stop(stop));
                    let (loaded, fresh) = match loaded {
                        Some(loaded) => (loaded, false),
                        None => match load(&runtime, &data, &record) {
                            Ok(loaded) => (Arc::new(loaded), true),
                            Err(why) => {
                                return GraphRan {
                                    outcome: Err(Failure::Stopped(why)),
                                    loaded: None,
                                    log: Vec::new(),
                                };
                            }
                        },
                    };
                    let outcome = loaded.graph(host.clone(), &graph, Limits::GRAPH);
                    GraphRan {
                        outcome,
                        loaded: fresh.then(|| (record.fingerprint.clone(), loaded)),
                        log: host.take_events(),
                    }
                })
                .await
                .unwrap_or_else(|_| GraphRan {
                    outcome: Err(Failure::Stopped(
                        tr!("the plugin's run failed unexpectedly").to_string(),
                    )),
                    loaded: None,
                    log: Vec::new(),
                })
            },
            move |ran| {
                Message::Plugin(PluginMsg::GraphBuilt {
                    stamp: stamp.clone(),
                    map: id.clone(),
                    rev,
                    fingerprint: done_fp.clone(),
                    serial,
                    ran: Arc::new(std::sync::Mutex::new(Some(ran))),
                })
            },
        )
    }

    /// A plugin graph's build ended: kept if it is still for the copy of
    /// the plugin installed, drawn if it is the map shown, and built again
    /// if the project moved while it ran.
    fn on_plugin_graph_built(
        &mut self,
        map: String,
        rev: u64,
        fingerprint: String,
        serial: u64,
        ran: GraphRan,
    ) -> Task<Message> {
        // Stopped because it was no longer shown: nothing to say of it, and
        // built again when next shown — unless a newer build of it runs.
        if matches!(&ran.outcome, Err(Failure::Stopped(why)) if why == clew_plugin::runtime::STOPPED_BY_USER)
        {
            if let Some(entry) = self
                .proj
                .plugin_maps
                .get_mut(&map)
                .filter(|m| m.fingerprint == fingerprint && m.serial == serial)
                && !entry.building
            {
                entry.rev = u64::MAX;
            }
            return Task::none();
        }
        let plugin = map_of(&map).map(|(plugin, _)| plugin.to_string());
        // A build of a copy replaced meanwhile: what it printed, and its
        // loaded code, are not the new copy's — its log read as the new
        // build's, freshly emptied.
        let current = plugin
            .as_deref()
            .and_then(|p| self.plugins.get(p))
            .is_some_and(|p| p.record.fingerprint == fingerprint);
        if let Some(plugin) = plugin.as_ref().filter(|_| current) {
            for HostEvent::Log(level, line) in ran.log {
                self.plugins.note(plugin, level, line);
            }
        }
        if let Some((fp, loaded)) = ran.loaded.filter(|_| current) {
            self.plugins.loaded.insert(fp, loaded);
        }
        // A build since started for this graph is the one it waits for:
        // what an earlier one gave (stopped as it ended) is not shown, and
        // does not end the newer one, which then went on unstoppable while
        // a third was asked for beside it.
        if self
            .proj
            .plugin_maps
            .get(&map)
            .is_some_and(|m| m.fingerprint == fingerprint && m.serial != serial)
        {
            return Task::none();
        }
        // Not built at all: turned off, removed or replaced since this
        // window read the list. The build is over, and the list is read
        // again, which takes the graph away or builds the new one's.
        if let Err(failure) = &ran.outcome
            && is_withdrawn(failure)
        {
            if let Some(entry) = self
                .proj
                .plugin_maps
                .get_mut(&map)
                .filter(|m| m.fingerprint == fingerprint)
            {
                entry.building = false;
                if matches!(failure, Failure::Stopped(why) if why == WITHDRAWN[3]) {
                    // The list unreadable: said where the graph is, with
                    // Try again, and built again when the files change —
                    // not asked for again at once, and refused again, for
                    // good.
                    entry.failed = Some(failure_words(failure));
                    entry.rev = rev;
                } else {
                    // Not built: asked for again when next shown, not
                    // taken as built for these files.
                    entry.rev = u64::MAX;
                }
            }
            return self.reload_plugins();
        }
        let decl = plugin
            .as_deref()
            .and_then(|p| self.plugins.get(p))
            .filter(|p| p.record.enabled && p.record.fingerprint == fingerprint)
            .and_then(|p| map_of(&map).and_then(|(_, g)| p.manifest.graph(g)).cloned());
        let Some(decl) = decl else {
            // Removed meanwhile: what it saved as it built goes too.
            if let Some(plugin) = plugin.as_deref()
                && self.plugins.get(plugin).is_none()
            {
                removed_store_of(plugin);
            }
            // The plugin was turned off, removed or replaced meanwhile: its
            // result is not shown, and the build is over all the same.
            // Left marked building, turned on again the graph waited for a
            // build no one would start, and could not be rebuilt.
            if let Some(entry) = self
                .proj
                .plugin_maps
                .get_mut(&map)
                .filter(|m| m.fingerprint == fingerprint)
            {
                entry.building = false;
                // Its result thrown away is no build: turned on again, it
                // is asked for again. Left at this revision, the graph
                // said "Building this graph…" for good — nothing started
                // one, and Try again is offered for a failure only.
                entry.rev = u64::MAX;
            }
            return Task::none();
        };
        let Some(entry) = self
            .proj
            .plugin_maps
            .get_mut(&map)
            .filter(|m| m.fingerprint == fingerprint)
        else {
            return Task::none();
        };
        entry.building = false;
        entry.rev = rev;
        match ran.outcome {
            Ok(graph) => {
                entry.built = Some(project_map(map.clone(), &decl, graph));
                entry.failed = None;
                entry.said = None;
                if let Some(plugin) = &plugin {
                    self.plugins.graph_failed.remove(plugin);
                    self.plugins.damaged.remove(plugin);
                }
            }
            Err(failure) => {
                if let Some(plugin) = &plugin {
                    // Why it drew nothing, in its own words, is its answer:
                    // it works.
                    if let Failure::Plugin(said) = &failure {
                        self.plugins.note(plugin, Level::Info, said.clone());
                        self.plugins.graph_failed.remove(plugin);
                        self.plugins.damaged.remove(plugin);
                    } else if matches!(&failure, Failure::Stopped(why) if why == clew_plugin::store::CHANGED)
                    {
                        self.plugins
                            .note(plugin, Level::Error, failure_words(&failure));
                        self.plugins.graph_failed.remove(plugin);
                        self.plugins.damaged.insert(plugin.clone());
                    } else {
                        self.plugins
                            .note(plugin, Level::Error, failure_words(&failure));
                        self.plugins.graph_failed.insert(plugin.clone());
                    }
                }
                let entry = self.proj.plugin_maps.get_mut(&map).expect("kept above");
                match failure {
                    // Said in its own words: shown as its answer, not as
                    // "could not be built" with its log to see — its row
                    // says nothing is wrong. What it drew before is not
                    // what it says now.
                    Failure::Plugin(said) => {
                        entry.said = Some(said);
                        entry.failed = None;
                        entry.built = None;
                    }
                    other => entry.failed = Some(failure_words(&other)),
                }
            }
        }
        let composed = self.compose_maps();
        let shown = self
            .selected_map_index()
            .and_then(|at| self.proj.maps.get(at))
            .is_some_and(|m| m.id == map);
        let relayout = match shown && self.proj.overlay == Some(Overlay::ProjectMaps) {
            true => self.refresh_graph_layout(),
            false => Task::none(),
        };
        Task::batch([composed, relayout, self.ensure_plugin_map()])
    }

    /// Stop the plugin graphs being built, but the one `keep`: not shown
    /// any more, each was built on for up to a minute, with its memory.
    /// Each is asked for again when next shown.
    pub(crate) fn stop_plugin_map_builds(&mut self, keep: Option<&str>) {
        for (id, map) in self.proj.plugin_maps.iter_mut() {
            if !map.building || keep == Some(id.as_str()) {
                continue;
            }
            if let Some(stop) = map.stop.take() {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            map.building = false;
            map.rev = u64::MAX;
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
    pub(crate) fn invocation(&self, input: Option<String>, at: Option<Spot>) -> Invocation {
        // From the menu: the pane and line clicked. Otherwise the code
        // being read, at its caret.
        let (viewer, line) = match at {
            Some(spot) => {
                let viewer = self.proj.panes.get(spot.pane).and_then(Option::as_ref);
                (viewer, viewer.map(|_| spot.line))
            }
            None => {
                let viewer = self.active_viewer();
                (viewer, viewer.and_then(|v| v.caret).map(|(line, _)| line))
            }
        };
        Invocation {
            file: viewer.map(|v| v.rel.clone()).filter(|r| !r.is_empty()),
            line: line.map(|line| (line + 1).min(u32::MAX as usize) as u32),
            selection: viewer.and_then(|v| v.selected_text()),
            input,
        }
    }

    /// Run plugin `id`'s command `command`, asking first for what it asks.
    fn run_plugin(
        &mut self,
        id: &str,
        command: &str,
        input: Option<String>,
        at: Option<Spot>,
        context: Option<Invocation>,
    ) -> Task<Message> {
        // Turned off (here, or in another window as a page or a failed run
        // was left to run again): said so, with its row, where it is turned
        // on again — not a line in passing with nowhere to go.
        if let Some(off) = self.plugins.get(id).filter(|p| !p.record.enabled).cloned() {
            let title = off
                .manifest
                .command(command)
                .map(|cmd| command_label(&off, cmd))
                .unwrap_or_else(|| off.name());
            let subject = title.clone();
            self.say_failure_again(
                move || {
                    tr!(
                        "{command} did not run: {reason}",
                        command = title.clone(),
                        reason = crate::i18n::lookup(WITHDRAWN[2])
                    )
                },
                None,
                FailureOf::PluginRun,
            );
            self.failure_in(Some(subject), Some(SettingsSpot::Plugin(id.to_string())));
            return Task::none();
        }
        let Some(plugin) = self.plugins.get(id).cloned() else {
            self.status = tr!("That plugin is not installed").into();
            return Task::none();
        };
        let Some(cmd) = plugin.manifest.command(command).cloned() else {
            self.status = tr!("{plugin} has no such command", plugin = plugin.name());
            return Task::none();
        };
        // Named with its plugin: two plugins' "List" are told apart on the
        // status line, the prompt and the page.
        let title = command_label(&plugin, &cmd);
        // What would stop the run, said before the user types for it.
        if let Some(running) = &self.plugins.running {
            self.status = tr!("{command} is still running", command = running.clone());
            return Task::none();
        }
        if self.proj.project.is_none() {
            self.status = tr!("Open a project to run {command}", command = title);
            return Task::none();
        }
        if let (Some(asks), None) = (&cmd.input, &input) {
            self.plugins.prompt = Some(Prompt {
                id: id.to_string(),
                command: command.to_string(),
                title,
                asks: asks.clone(),
                text: String::new(),
                at,
            });
            return iced::widget::operation::focus(crate::ui::plugin_prompt_id());
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
        // Run again on what it was first run on: the file a row opened
        // since is not what the page was about.
        let invocation = match context {
            Some(context) => Invocation {
                input: input.clone(),
                ..context
            },
            None => self.invocation(input.clone(), at),
        };
        let again = Again {
            id: id.to_string(),
            command: command.to_string(),
            input,
            at,
            context: Some(invocation.clone()),
        };
        let fingerprint = plugin.record.fingerprint.clone();
        let loaded = self.plugins.loaded.get(&plugin.record.fingerprint).cloned();
        let progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
        self.plugins.progress = Some(progress.clone());
        // Said by its chip on the status bar, with its Stop, for as long as
        // it runs: said on the status line, it went as soon as anything
        // else was said, and could not be stopped.
        self.plugins.running = Some(title.clone());
        self.plugins.running_again = Some(again.clone());
        // What was said before ("… was stopped") does not stand beside
        // the new run's chip, saying the opposite.
        self.status.clear();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.plugins.stop = Some(stop.clone());
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
                    if let Some(why) = withdrawn(&data, &id, &record) {
                        return Ran {
                            outcome: Err(Failure::Stopped(why.to_string())),
                            loaded: None,
                            log: Vec::new(),
                        };
                    }
                    let host = Arc::new(host(handle).with_progress(progress).with_stop(stop));
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
                    again: again.clone(),
                    fingerprint: fingerprint.clone(),
                    ran: Arc::new(std::sync::Mutex::new(Some(ran))),
                })
            },
        )
    }

    /// A run's end: show what it gave back.
    fn on_plugin_ran(
        &mut self,
        id: String,
        title: String,
        again: Again,
        fingerprint: String,
        ran: Ran,
    ) -> Task<Message> {
        self.plugins.running = None;
        self.plugins.running_again = None;
        self.plugins.progress = None;
        self.plugins.stop = None;
        // Replaced while it ran (its update installed meanwhile): what the
        // copy that ran wrote, and its loaded code, are not the new
        // build's — its log read as the new one's, its crash marked the new
        // one as not working.
        let current = self
            .plugins
            .get(&id)
            .is_some_and(|p| p.record.fingerprint == fingerprint);
        if current {
            for HostEvent::Log(level, line) in ran.log {
                self.plugins.note(&id, level, line);
            }
            if let Some((fingerprint, loaded)) = ran.loaded {
                self.plugins.loaded.insert(fingerprint, loaded);
            }
        }
        // Removed while it ran: what it gave back is not shown (a page
        // headed by no name), nor counted against a plugin that is gone.
        let Some(name) = self.plugins.get(&id).map(Installed::name) else {
            self.plugins.log.remove(&id);
            // What it saved while it ran went after its removal had taken
            // its store: a later install of the same id found it.
            removed_store_of(&id);
            self.status = match &ran.outcome {
                // Refused at its start: removed before it could run.
                Err(failure) if is_withdrawn(failure) => tr!(
                    "{command} did not run: {reason}",
                    command = title,
                    reason = tr!("it was removed")
                ),
                _ => tr!(
                    "{command} ran, but its plugin was removed meanwhile",
                    command = title
                ),
            };
            return Task::none();
        };
        // Stopped by the user: what it did so far is in its log; it did
        // not fail, and what it gave back is not shown.
        if matches!(&ran.outcome, Err(Failure::Stopped(why)) if why == clew_plugin::runtime::STOPPED_BY_USER)
        {
            self.status = tr!("{command} was stopped", command = title);
            return Task::none();
        }
        // Not run at all: turned off, removed or replaced since this window
        // read the list (in another copy of Clew). The list is read again;
        // nothing failed, and there is nothing to try again.
        if let Err(failure) = &ran.outcome
            && is_withdrawn(failure)
        {
            // The list read again first: what this window kept of the copy
            // that was there goes with it (`forget_copy_of`), and what is
            // said now, of the one there now, stays. Read after, it took
            // "did not run: it was updated" and its Try again with it.
            let reload = self.reload_plugins();
            if matches!(failure, Failure::Stopped(why) if why == WITHDRAWN[1]) {
                // Replaced since: the new build is there to run, one
                // click away rather than found again in the palette.
                self.say_failure_again(
                    move || {
                        tr!(
                            "{command} did not run: {reason}",
                            command = title.clone(),
                            reason = crate::i18n::lookup(WITHDRAWN[1])
                        )
                    },
                    Some(Message::Plugin(PluginMsg::RunAgain(again))),
                    FailureOf::PluginRun,
                );
            } else if matches!(failure, Failure::Stopped(why) if why == WITHDRAWN[3]) {
                // Its list unreadable: Settings → Plugins says why, at its
                // top, one click away; said no more once it reads again.
                self.say_failure_again(
                    move || {
                        tr!(
                            "{command} did not run: {reason}",
                            command = title.clone(),
                            reason = crate::i18n::lookup(WITHDRAWN[3])
                        )
                    },
                    None,
                    FailureOf::PluginRun,
                );
            } else if matches!(failure, Failure::Stopped(why) if why == WITHDRAWN[2]) {
                // Turned off in another window: its Settings row, where it
                // is turned on again, is one click away.
                let subject = title.clone();
                self.say_failure_again(
                    move || {
                        tr!(
                            "{command} did not run: {reason}",
                            command = title.clone(),
                            reason = crate::i18n::lookup(WITHDRAWN[2])
                        )
                    },
                    None,
                    FailureOf::PluginRun,
                );
                self.failure_in(Some(subject), Some(SettingsSpot::Plugin(id)));
            } else {
                self.status = tr!(
                    "{command} did not run: {reason}",
                    command = title,
                    reason = failure_words(failure)
                );
            }
            return reload;
        }
        // Its approved copy changed on disk: not run. Trying again fails the
        // same way, and its source is not what is wrong; installed again, it
        // is written anew — said so, here and in its Settings row. (The copy
        // of a build replaced while it ran is gone with it: said as that,
        // below; the new copy is not damaged.)
        if current
            && matches!(&ran.outcome, Err(Failure::Stopped(why)) if why == clew_plugin::store::CHANGED)
        {
            self.plugins.failed.remove(&id);
            self.plugins.damaged.insert(id.clone());
            self.plugins.note(
                &id,
                Level::Error,
                crate::i18n::lookup(clew_plugin::store::CHANGED).to_string(),
            );
            let subject = title.clone();
            self.say_failure_again(
                move || {
                    tr!(
                        "{command} did not run: {reason}",
                        command = title.clone(),
                        reason = crate::i18n::lookup(clew_plugin::store::CHANGED)
                    )
                },
                None,
                FailureOf::PluginRun,
            );
            // Its row says what fixes it.
            self.failure_in(Some(subject), Some(SettingsSpot::Plugin(id)));
            return Task::none();
        }
        // A failure of the copy replaced is not the new build's: the run is
        // offered again, on the new build, and nothing is marked failing.
        if !current && !matches!(ran.outcome, Ok(_) | Err(Failure::Plugin(_))) {
            self.say_failure_again(
                move || {
                    tr!(
                        "{command} was updated while it ran: run it again for the new version",
                        command = title.clone()
                    )
                },
                Some(Message::Plugin(PluginMsg::RunAgain(again))),
                FailureOf::PluginRun,
            );
            return Task::none();
        }
        // A command that finished, or that answered with a reason of its
        // own, works: the failure of its last run goes. A plugin's own
        // "The project has no git history" read as a crash, with Try
        // again (which failed the same way) and "It is not working".
        if !current {
            // An answer of the copy replaced says nothing of the new one.
        } else if matches!(ran.outcome, Ok(_) | Err(Failure::Plugin(_))) {
            self.forget_run_failure_of(&id);
            self.plugins.failed.remove(&id);
            // Its copy was read and ran: not damaged (a read that failed
            // once, or a copy installed again meanwhile).
            self.plugins.damaged.remove(&id);
        } else {
            self.plugins.failed.insert(id.clone());
        }
        // The page this very run refreshes (its ↻): what it showed is no
        // longer what the command gives once it answers in words. "No TODOs
        // in this project" under the twelve it listed before read as both.
        let its_page = self
            .proj
            .plugin_page
            .as_ref()
            .is_some_and(|page| page.again.as_ref() == Some(&again));
        match ran.outcome {
            Ok(Outcome::Show(view)) => {
                let mut page = PluginPage::new(id, name, view);
                page.again = Some(again);
                self.show_plugin_page(page);
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
                if its_page {
                    self.page_says(text.clone());
                }
                self.status = format!("{title}: {text}");
                Task::none()
            }
            Ok(Outcome::Nothing) => {
                if its_page {
                    self.page_says(tr!("It has nothing to show now.").to_string());
                }
                self.status = tr!("{command} is done", command = title);
                Task::none()
            }
            // What the plugin says why it did nothing is said as it wrote
            // it, as what it says when it did: its answer, not a failure.
            Err(Failure::Plugin(said)) => {
                if current {
                    self.plugins.note(&id, Level::Info, said.clone());
                }
                if its_page {
                    self.page_says(said.clone());
                }
                self.status = format!("{title}: {said}");
                Task::none()
            }
            Err(failure) => {
                self.plugins
                    .note(&id, Level::Error, failure_words(&failure));
                // It stands, with Try again (the same run) and Settings
                // (Settings → Plugins, its log and what fixes it): said
                // in passing, it was gone in seconds, with no way on.
                let subject = title.clone();
                self.say_failure_again(
                    move || {
                        tr!(
                            "{command} did not finish: {reason}",
                            command = title.clone(),
                            reason = failure_words(&failure)
                        )
                    },
                    Some(Message::Plugin(PluginMsg::RunAgain(again))),
                    FailureOf::PluginRun,
                );
                self.failure_in(Some(subject), Some(SettingsSpot::Plugin(id)));
                Task::none()
            }
        }
    }

    /// Take a run of plugin `id` that did not finish off the status line,
    /// and only one of it: another plugin's finishing said nothing of it.
    pub(crate) fn forget_run_failure_of(&mut self, id: &str) {
        let of_it = self.failure_notice.as_ref().is_some_and(|f| {
            f.of == FailureOf::PluginRun
                && match &f.retry {
                    Some(Message::Plugin(PluginMsg::RunAgain(again))) => again.id == id,
                    // A damaged copy's, or one turned off: nothing to try
                    // again; its own by the row it points to. Another
                    // plugin's, or the unreadable list's, is not.
                    None => matches!(&f.spot, Some(SettingsSpot::Plugin(p)) if p == id),
                    _ => false,
                }
        });
        if of_it {
            self.forget_failure_of(FailureOf::PluginRun);
        }
    }

    /// Forget what this window kept of plugin `id`'s copy, replaced,
    /// installed again or removed — here or in another window: its log, its
    /// failures and damaged mark, a run of it that did not finish, and why
    /// its graphs could not be built (built again when next shown).
    /// `removed`: its page, the page to go back to and its question go too.
    pub(crate) fn forget_copy_of(&mut self, id: &str, removed: bool) {
        self.forget_run_failure_of(id);
        self.plugins.forget_copy(id);
        for (map, entry) in self.proj.plugin_maps.iter_mut() {
            if map_of(map).is_some_and(|(plugin, _)| plugin == id) && !entry.building {
                entry.rev = u64::MAX;
                entry.failed = None;
            }
        }
        if removed {
            if self.proj.plugin_page.as_ref().is_some_and(|p| p.id == id) {
                self.proj.plugin_page = None;
            }
            if self
                .proj
                .last_plugin_page
                .as_ref()
                .is_some_and(|p| p.id == id)
            {
                self.proj.last_plugin_page = None;
            }
            if self.plugins.prompt.as_ref().is_some_and(|p| p.id == id) {
                self.plugins.prompt = None;
            }
        }
    }

    /// The plugin page shown says `words` in place of what it showed, under
    /// its title, and can still be run again.
    fn page_says(&mut self, words: String) {
        use clew_plugin::runtime::{Block, Notice};
        let Some(page) = self.proj.plugin_page.as_mut() else {
            return;
        };
        let view = View {
            title: page.view.title.clone(),
            blocks: vec![Block::Notice(Notice {
                level: Level::Info,
                text: words,
            })],
        };
        let mut said = PluginPage::new(page.id.clone(), page.plugin.clone(), view);
        said.again = page.again.take();
        *page = said;
    }

    /// Show `page` over the code, in place of any other page.
    pub(crate) fn show_plugin_page(&mut self, page: PluginPage) {
        self.proj.overview.showing = false;
        self.proj.stats.showing = false;
        self.proj.weak.showing = false;
        self.proj.glossary.showing = false;
        self.proj.docs.page = None;
        self.proj.plugin_page = Some(page);
        self.proj.last_plugin_page = None;
    }

    /// Ask the user whether to install `prepared`.
    pub(crate) fn offer_install(&mut self, prepared: Prepared, origin: Source, reply: Reply) {
        let id = &prepared.inspected.manifest.plugin.id;
        let previous = self.plugins.get(id).cloned();
        let unreadable = self
            .plugins
            .unreadable
            .iter()
            .find(|(at, _)| at == id)
            .map(|(_, name)| name.clone());
        let replaced = self.plugins.install.offer(PendingInstall {
            bytes: prepared.bytes,
            inspected: prepared.inspected,
            origin,
            previous,
            unreadable,
            reply,
        });
        // The build asked about before, of the same plugin, is not asked
        // about any more: its asker is told so, not left waiting for an
        // answer that would never come (an Ask turn hung on it).
        if let Some(replaced) = replaced {
            self.answer_install(
                &replaced.reply,
                Err("A newer build of this plugin was offered to the user in its place: install that one, or build and install again.".into()),
                false,
            );
        }
    }

    /// Tell whoever asked for an install how it went: installed with this
    /// fingerprint, said so in these words for an agent; failed; or
    /// `declined` by the user.
    fn answer_install(
        &mut self,
        reply: &Reply,
        answer: Result<Installation, String>,
        declined: bool,
    ) {
        match reply {
            Reply::None => {}
            Reply::AskTool { stream, call } => {
                let (content, is_error) = match (&answer, declined) {
                    (_, true) => ("The user chose not to install it.".to_string(), false),
                    (Ok(done), _) => (done.words.clone(), false),
                    (Err(why), _) => (why.clone(), true),
                };
                let state = match (&answer, declined) {
                    (Ok(_), false) => crate::app::model::StepState::Done,
                    _ => crate::app::model::StepState::Failed,
                };
                self.finish_plugin_step(*stream, *call, "plugin_install", state);
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
                    (Ok(done), _) => clew_plugin::requests::Answer::Installed {
                        fingerprint: done.fingerprint,
                        palette: done.palette,
                    },
                    (Err(why), _) => clew_plugin::requests::Answer::Failed { why },
                };
                if let Ok(data) = data() {
                    let _ = clew_plugin::requests::answer(&data, nonce, &answer);
                }
                self.plugins.handling.remove(nonce);
            }
        }
    }

    /// The window is closing: what `clew-server mcp` asked of it is
    /// answered now — an install card unanswered, a try under way — so the
    /// agent hears the window went. It waited up to fifteen minutes and was
    /// told the user did not answer.
    pub(crate) fn release_plugin_requests(&mut self) {
        let Ok(data) = data() else {
            return;
        };
        let why = "The Clew window it was asked in was closed.".to_string();
        let mut nonces: Vec<String> = self
            .plugins
            .install
            .withdraw(|p| matches!(p.reply, Reply::Request(_)))
            .into_iter()
            .filter_map(|p| match p.reply {
                Reply::Request(nonce) => Some(nonce),
                _ => None,
            })
            .collect();
        nonces.extend(self.plugins.handling.drain());
        nonces.sort();
        nonces.dedup();
        for nonce in nonces {
            let _ = clew_plugin::requests::answer(
                &data,
                &nonce,
                &clew_plugin::requests::Answer::Failed { why: why.clone() },
            );
        }
    }

    /// Run `built`'s command for an agent trying it: with the permissions
    /// its code needs, read and git at most, on the open project.
    pub(crate) fn try_for_agent(
        &mut self,
        built: clew_plugin::build::Built,
        trial: Trial,
        reply: Reply,
    ) -> Task<Message> {
        let Trial {
            command,
            graph,
            file,
            line,
            input,
        } = trial;
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
                    // Tried, not installed: a store of its own, emptied
                    // after, and no compiled copy kept.
                    let loaded = runtime.load_trial(&built.bytes, &built.inspected, granted)?;
                    // A graph is built as Project Graphs builds it; a
                    // command runs as the palette runs it.
                    let outcome = match &graph {
                        Some(graph) => loaded
                            .graph(host.clone(), graph, Limits::GRAPH)
                            .map(|g| clew_plugin::tools::describe_graph(&g)),
                        None => loaded
                            .run(host.clone(), &command, &invocation, Limits::COMMAND)
                            .map(|o| clew_plugin::tools::describe_outcome(&o)),
                    };
                    let log: Vec<String> = host
                        .take_events()
                        .into_iter()
                        .map(|HostEvent::Log(level, line)| {
                            let level = match level {
                                Level::Info => "info",
                                Level::Warning => "warning",
                                Level::Error => "error",
                            };
                            format!("[{level}] {line}")
                        })
                        .collect();
                    let mut answer = match outcome {
                        Ok(said) => said,
                        Err(Failure::Plugin(why)) => format!("It answered with an error: {why}"),
                        Err(other) => format!("It did not finish: {other}"),
                    };
                    if !log.is_empty() {
                        answer.push_str("\n\nIts log:\n");
                        answer.push_str(&log.join("\n"));
                    }
                    // What an agent is handed, cut to what it can be: over
                    // what `clew-server mcp` reads back, it waited the whole
                    // wait and was told the try did not finish.
                    Ok(cut_answer(answer))
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
        self.push_plugin_step(stream, call, &tool);
        let chip = tool.clone();
        let plugin = args
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::to_string);
        let cancel = self
            .plugins
            .tool_cancels
            .entry(stream)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    clew_plugin::tools::call(&data, &tool, &args, &cancel)
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
                    tool: chip.clone(),
                    plugin: plugin.clone(),
                    answer: Arc::new(std::sync::Mutex::new(Some(step))),
                })
            },
        )
    }

    /// A step chip for a plugin tool the agent called, running until its
    /// answer comes (`finish_plugin_step`): it was ticked as it was called —
    /// "Built a plugin" green as a build of minutes began, and green still
    /// when it failed; "Asked to install a plugin" ticked while the card
    /// waited, and after the user said no.
    fn push_plugin_step(&mut self, stream: u64, call: u64, tool: &str) {
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream) {
            turn.extras
                .step_ids
                .insert(format!("plugin:{call}"), turn.steps.len());
            turn.steps.push(crate::app::model::AgentStep {
                tool: "plugin".into(),
                title: plugin_step_title(tool, crate::app::model::StepState::Running),
                refs: Vec::new(),
                state: crate::app::model::StepState::Running,
            });
        }
    }

    /// Plugin tool call `call` of `stream`'s turn stands as `state` now:
    /// done is said in the past, as the agent's other steps are.
    fn finish_plugin_step(
        &mut self,
        stream: u64,
        call: u64,
        tool: &str,
        state: crate::app::model::StepState,
    ) {
        self.set_plugin_step(stream, call, state, plugin_step_title(tool, state));
    }

    /// Plugin tool call `call` of `stream`'s turn stands as `state`, said
    /// as `title`.
    fn set_plugin_step(
        &mut self,
        stream: u64,
        call: u64,
        state: crate::app::model::StepState,
        title: String,
    ) {
        if let Some(turn) = self.proj.ask_turns.iter_mut().find(|t| t.stream == stream)
            && let Some(&at) = turn.extras.step_ids.get(&format!("plugin:{call}"))
            && let Some(step) = turn.steps.get_mut(at)
        {
            step.state = state;
            step.title = title;
        }
    }

    /// Handle the requests found: try, or ask about installing.
    fn on_requests(&mut self, found: &[(String, clew_plugin::requests::Request)]) -> Task<Message> {
        use clew_plugin::requests::Ask;
        let Ok(data) = data() else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        for (nonce, request) in found {
            // A try is for the window with its project open: another window
            // leaves it for that one. `clew-server mcp` runs on this machine,
            // so a remote project is never the one it means.
            if let Ask::Try { root, .. } = &request.ask
                && !self.has_open(root)
            {
                continue;
            }
            if self.plugins.handling.contains(nonce) || !clew_plugin::requests::claim(&data, nonce)
            {
                continue;
            }
            self.plugins.handling.insert(nonce.clone());
            let (plugin, sha256) = match &request.ask {
                Ask::Try { plugin, sha256, .. } | Ask::Install { plugin, sha256 } => {
                    (plugin.clone(), sha256.clone())
                }
            };
            // The build named by its digest, from Clew's own build folder:
            // read (a file of up to 32 MB) and checked off the window.
            let data = data.clone();
            let nonce = nonce.clone();
            let request = Arc::new(request.clone());
            tasks.push(Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        clew_plugin::tools::last_build(&data, &plugin).and_then(|built| {
                            if clew_plugin::inspect::hex(&built.inspected.sha256) == sha256 {
                                Ok(built)
                            } else {
                                Err("the plugin was built again since; ask again".to_string())
                            }
                        })
                    })
                    .await
                    .unwrap_or_else(|_| Err("reading the build failed unexpectedly".into()))
                },
                move |built| {
                    Message::Plugin(PluginMsg::RequestRead {
                        nonce: nonce.clone(),
                        request: request.clone(),
                        built: Arc::new(std::sync::Mutex::new(Some(built))),
                    })
                },
            ));
        }
        Task::batch(tasks)
    }

    /// Take away the install cards whose agent stopped waiting (it gave up
    /// after a quarter of an hour, or its client was stopped): it was told
    /// the install was withdrawn, and the card stood on — Install, pressed
    /// later, installed it after all.
    pub(crate) fn drop_withdrawn_installs(&mut self) {
        let Ok(data) = data() else {
            return;
        };
        let gone = self.plugins.install.withdraw(|p| {
            matches!(&p.reply, Reply::Request(nonce) if !clew_plugin::requests::is_taken(&data, nonce))
        });
        for pending in gone {
            if let Reply::Request(nonce) = &pending.reply {
                self.plugins.handling.remove(nonce);
            }
            self.status = tr!(
                "{plugin} was not installed: the agent that asked stopped waiting",
                plugin = pending
                    .inspected
                    .manifest
                    .name_in(crate::i18n::current().code())
            );
        }
    }

    /// Whether the project at `root` is the one open here, on this machine
    /// — where `clew-server mcp`'s agents are.
    fn has_open(&self, root: &str) -> bool {
        self.local_project_state()
            && self
                .proj
                .project
                .as_ref()
                .is_some_and(|p| same_folder(&p.root, Path::new(root)))
    }

    /// A request's build read: tried, or asked about, as it asked; or
    /// answered at once with why not.
    fn on_request_read(
        &mut self,
        nonce: String,
        request: &clew_plugin::requests::Request,
        built: Result<clew_plugin::build::Built, String>,
    ) -> Task<Message> {
        use clew_plugin::requests::{Answer, Ask};
        let Ok(data) = data() else {
            return Task::none();
        };
        let reply = Reply::Request(nonce.clone());
        // The project it was claimed for may have been put away while the
        // build was read: tried then, the plugin read another project, and
        // what it read went to the agent of the first.
        let built = match built {
            Ok(_) if matches!(&request.ask, Ask::Try { root, .. } if !self.has_open(root)) => {
                Err("The project the plugin was to be tried on was closed in Clew meanwhile. Open it again, then try again.".to_string())
            }
            built => built,
        };
        let built = match built {
            Ok(built) => built,
            Err(why) => {
                let _ = clew_plugin::requests::answer(&data, &nonce, &Answer::Failed { why });
                self.plugins.handling.remove(&nonce);
                return Task::none();
            }
        };
        {
            match &request.ask {
                Ask::Try {
                    command,
                    graph,
                    file,
                    line,
                    input,
                    ..
                } => self.try_for_agent(
                    built,
                    Trial {
                        command: command.clone(),
                        graph: graph.clone(),
                        file: file.clone(),
                        line: *line,
                        input: input.clone(),
                    },
                    reply,
                ),
                Ask::Install { .. } => {
                    self.offer_install(
                        Prepared {
                            bytes: Arc::new(built.bytes),
                            inspected: built.inspected,
                        },
                        Source::Agent(request.agent.clone()),
                        reply,
                    );
                    // An agent outside Clew asked: the window may be behind
                    // its terminal, or minimized, and the card waited there
                    // unseen until the agent gave up. It asks for attention
                    // (the Dock, the taskbar), without taking the keyboard.
                    match self.main_window.filter(|_| !self.window_focused) {
                        Some(window) => iced::window::request_user_attention(
                            window,
                            Some(iced::window::UserAttention::Informational),
                        ),
                        None => Task::none(),
                    }
                }
            }
        }
    }

    /// Handle a [`PluginMsg`].
    pub(crate) fn update_plugins(&mut self, message: PluginMsg) -> Task<Message> {
        match message {
            PluginMsg::Run { id, command } => self.run_plugin(&id, &command, None, None, None),
            PluginMsg::RunFromMenu { id, command } => {
                let Some(menu) = self.proj.context_menu.take() else {
                    return Task::none();
                };
                let at = Spot {
                    pane: menu.pane,
                    line: menu.line,
                };
                self.run_plugin(&id, &command, None, Some(at), None)
            }
            // Its chip reads how far it has come as it is drawn.
            PluginMsg::ProgressTick => Task::none(),
            PluginMsg::ShowInSettings(spot) => {
                self.plugins.reveal_in_settings = Some(spot);
                self.update(Message::Settings(crate::app::message::SettingsMsg::OpenAt(
                    crate::app::state::SettingsPage::Plugins,
                )))
            }
            PluginMsg::StopRun => {
                if let Some(stop) = &self.plugins.stop {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Task::none()
            }
            PluginMsg::PromptEdit(text) => {
                if let Some(prompt) = self.plugins.prompt.as_mut() {
                    prompt.text = text;
                }
                Task::none()
            }
            PluginMsg::PromptRun => {
                // Nothing typed is nothing to run on: the question stays,
                // its field still focused (Run said by voice, or Enter).
                if self
                    .plugins
                    .prompt
                    .as_ref()
                    .is_some_and(|p| p.text.trim().is_empty())
                {
                    return iced::widget::operation::focus(crate::ui::plugin_prompt_id());
                }
                let Some(prompt) = self.plugins.prompt.take() else {
                    return Task::none();
                };
                self.run_plugin(
                    &prompt.id,
                    &prompt.command,
                    Some(prompt.text),
                    prompt.at,
                    None,
                )
            }
            PluginMsg::PromptCancel => {
                self.plugins.prompt = None;
                Task::none()
            }
            PluginMsg::Ran {
                id,
                title,
                again,
                fingerprint,
                ran,
                ..
            } => {
                let Some(ran) = ran.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    return Task::none();
                };
                let read = list_read_again(&self.plugins, &ran.outcome);
                let shown = self.on_plugin_ran(id, title, again, fingerprint, ran);
                match read {
                    true => Task::batch([shown, self.reload_plugins()]),
                    false => shown,
                }
            }
            PluginMsg::RunAgain(again) => self.run_plugin(
                &again.id,
                &again.command,
                again.input,
                again.at,
                again.context,
            ),
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
                // Opening the file puts the page away, as it does every page
                // — kept, to go back to for the next row (the trail's Back
                // to): put away for good, the next row of a list of TODOs
                // meant running the command again.
                let page = self.proj.plugin_page.take();
                let opened = self.open_file(root.join(&at.path), line, true);
                self.proj.last_plugin_page = page;
                opened
            }
            PluginMsg::BackToPage => {
                if let Some(page) = self.proj.last_plugin_page.take() {
                    self.show_plugin_page(page);
                }
                Task::none()
            }
            PluginMsg::ClosePage => {
                self.proj.plugin_page = None;
                self.proj.last_plugin_page = None;
                Task::none()
            }
            PluginMsg::InstallFromFile => {
                self.plugins.failure = None;
                crate::app::tasks::pick_plugin_file(self.main_window).map(|picked| match picked {
                    Ok(path) => Message::Plugin(PluginMsg::Picked(path)),
                    Err(_) => Message::Window(WindowMsg::NoFileDialog),
                })
            }
            PluginMsg::Picked(None) => Task::none(),
            PluginMsg::Picked(Some(path)) => {
                let origin = Source::File(path.to_string_lossy().into_owned());
                let shown = file_name(&path);
                let could_not_read = move |reason: String| -> String {
                    tr!(
                        "Could not read {file}: {reason}",
                        file = shown.clone(),
                        reason = reason
                    )
                };
                Task::perform(
                    async move {
                        let unread = could_not_read.clone();
                        tokio::task::spawn_blocking(move || -> Result<Prepared, String> {
                            let bytes = read_plugin_file(&path).map_err(&could_not_read)?;
                            let inspected = clew_plugin::inspect(&bytes)
                                .map_err(|why| not_installable(&why))?;
                            Ok(Prepared {
                                bytes: Arc::new(bytes),
                                inspected,
                            })
                        })
                        .await
                        .unwrap_or_else(|_| {
                            Err(unread(tr!("it stopped on an internal error").into()))
                        })
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
                        self.plugins.failure = Some(why.clone());
                        self.answer_install(&reply, Err(why.clone()), false);
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
                // Its agent may have stopped waiting a moment ago: it was
                // told the install was withdrawn, and is not to find it
                // installed after all.
                if let Reply::Request(nonce) = &pending.reply
                    && !clew_plugin::requests::is_taken(&data, nonce)
                {
                    self.plugins.handling.remove(nonce);
                    self.status = tr!(
                        "{plugin} was not installed: the agent that asked stopped waiting",
                        plugin = pending
                            .inspected
                            .manifest
                            .name_in(crate::i18n::current().code())
                    );
                    return Task::none();
                }
                let id = pending.inspected.manifest.plugin.id.clone();
                let reply = pending.reply.clone();
                let update = pending
                    .previous
                    .as_ref()
                    .filter(|p| p.record.fingerprint != pending.inspected.fingerprint())
                    .map(|p| p.manifest.plugin.version.clone());
                let replaced = pending.previous.is_none() && pending.unreadable.is_some();
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
                            update,
                            replaced,
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
            PluginMsg::Installed {
                id,
                reply,
                update,
                replaced,
                result,
            } => {
                if result.is_ok() {
                    store_changed();
                }
                let composed = self.reload_plugins();
                let name = self
                    .plugins
                    .get(&id)
                    .map(Installed::name)
                    .unwrap_or(id.clone());
                let answer = match result {
                    Ok(fingerprint) => {
                        self.forget_failure_of(FailureOf::PluginInstall);
                        // A new build: whether it works is for its runs to say,
                        // and its log starts empty. The last build's crash,
                        // kept under the new version, read as the new one's.
                        // So does a run of the build replaced that did not
                        // finish: it stood on the status bar beside the
                        // new build, its Try again and Settings about the
                        // old one. (Read again above, it is forgotten
                        // already; said here for an install of the same
                        // second, which reads as the same record.)
                        self.forget_copy_of(&id, false);
                        let manifest = self.plugins.get(&id).map(|p| p.manifest.clone());
                        let (commands, graphs) = manifest
                            .as_ref()
                            .map_or((0, 0), |m| (m.commands.len(), m.graphs.len()));
                        // Where what it added is, as it added it.
                        // Said in few words: said in many ("…: its commands
                        // are in the command palette, its graphs in Project
                        // Graphs"), it was cut beside the status bar's chips.
                        let version = manifest
                            .as_ref()
                            .map(|m| m.plugin.version.clone())
                            .unwrap_or_default();
                        self.status = match (commands > 0, graphs > 0) {
                            // Where its commands are is known already. A new
                            // build of the same version is not "updated to"
                            // the version it was.
                            _ if update.as_ref().is_some_and(|was| *was == version) => {
                                tr!("{plugin} is updated", plugin = name.clone())
                            }
                            _ if replaced
                                || update.as_ref().is_some_and(|was| {
                                    clew_plugin::manifest::version_order(&version, was)
                                        == std::cmp::Ordering::Less
                                }) =>
                            {
                                tr!(
                                    "{plugin} is replaced with version {version}",
                                    plugin = name.clone(),
                                    version = version.clone()
                                )
                            }
                            _ if update.is_some() => tr!(
                                "{plugin} is updated to version {version}",
                                plugin = name.clone(),
                                version = version
                            ),
                            (true, true) => tr!(
                                "{plugin} is installed, in the command palette and Project Graphs",
                                plugin = name.clone()
                            ),
                            (false, true) => tr!(
                                "{plugin} is installed, in Project Graphs",
                                plugin = name.clone()
                            ),
                            _ => tr!(
                                "{plugin} is installed, in the command palette",
                                plugin = name.clone()
                            ),
                        };
                        // Its commands by the names the palette gives them:
                        // one named as another plugin's command is, or as
                        // one of Clew's own, carries its plugin's id there.
                        let palette = self.palette_names_of(&id);
                        let mut words = manifest
                            .map(|m| m.installed_words_as(&palette))
                            .unwrap_or_else(|| format!("The user installed {name}."));
                        if commands > 0 {
                            words.push_str(&format!(
                                " You can run one with {}, by its palette name.",
                                crate::app::windowtools::RUN_COMMAND
                            ));
                        }
                        Ok(Installation {
                            fingerprint,
                            words,
                            palette,
                        })
                    }
                    Err(why) => {
                        self.plugins.failure = Some(tr!(
                            "{plugin} could not be installed: {reason}",
                            plugin = name.clone(),
                            reason = reason_words(&why)
                        ));
                        // Said where the user is, too: after Install on an
                        // agent's card, Settings was not open, and the card
                        // was gone with nothing said.
                        let (plugin, reason) = (name.clone(), why.clone());
                        self.say_failure_again(
                            move || {
                                tr!(
                                    "{plugin} could not be installed: {reason}",
                                    plugin = plugin.clone(),
                                    reason = reason_words(&reason)
                                )
                            },
                            None,
                            FailureOf::PluginInstall,
                        );
                        // An update that failed: its row is there, with its
                        // log.
                        let spot = self
                            .plugins
                            .get(&id)
                            .map(|_| SettingsSpot::Plugin(id.clone()));
                        self.failure_in(Some(name.clone()), spot);
                        Err(why)
                    }
                };
                self.answer_install(&reply, answer, false);
                composed
            }
            PluginMsg::SetEnabled(id, on) => {
                match data().and_then(|d| clew_plugin::store::set_enabled(&d, &id, on)) {
                    // Still as it was: said in Settings, and where it was
                    // asked (Project Graphs' Turn on said nothing).
                    Err(why) => {
                        let plugin = self
                            .plugins
                            .get(&id)
                            .map(Installed::name)
                            .unwrap_or_else(|| id.clone());
                        self.status = match on {
                            true => tr!(
                                "{plugin} could not be turned on: {reason}",
                                plugin = plugin,
                                reason = why.clone()
                            ),
                            false => tr!(
                                "{plugin} could not be turned off: {reason}",
                                plugin = plugin,
                                reason = why.clone()
                            ),
                        };
                        self.plugins.failure = Some(why);
                    }
                    // A run of it that did not finish stood on, its Try
                    // again a run of a plugin turned off; and turned on, "it
                    // is turned off" is said of it no more.
                    Ok(()) => {
                        self.forget_run_failure_of(&id);
                        store_changed();
                    }
                }
                self.reload_plugins()
            }
            PluginMsg::StoreChanged => self.reload_plugins(),
            // Looked at again where it was looked at: a window that never
            // showed the row has nothing stale to show.
            PluginMsg::ToolchainChanged => match self.plugins.building {
                Building::Unknown => Task::none(),
                _ => self.check_toolchain(),
            },
            PluginMsg::Link(url) => {
                // A plugin read the project and wrote this page: a web
                // address can carry what it read away, so it is shown in
                // full before anything opens it.
                if url.starts_with("http://") || url.starts_with("https://") {
                    self.plugins.confirm = Some(Confirm {
                        title: tr!("Open this address in your browser?").to_string(),
                        detail: url.clone(),
                        yes: tr!("Open").to_string(),
                        destructive: false,
                        then: Then::OpenLink(url),
                    });
                    return Task::none();
                }
                // A link to a file opens it as a row does: the page is kept
                // to go back to ("Back to"), not lost with no way back.
                let page = self.proj.plugin_page.clone();
                let opened = self.on_open_link(url);
                if self.proj.plugin_page.is_none() && page.is_some() {
                    self.proj.last_plugin_page = page;
                }
                opened
            }
            PluginMsg::AskRemove(id) => {
                // One this Clew cannot read is named as its row names it.
                let name = self
                    .plugins
                    .get(&id)
                    .map(Installed::name)
                    .or_else(|| {
                        self.plugins
                            .unreadable
                            .iter()
                            .find(|(at, _)| *at == id)
                            .map(|(_, name)| name.clone())
                    })
                    .unwrap_or_else(|| id.clone());
                self.plugins.confirm = Some(Confirm {
                    title: tr!("Remove “{plugin}”?", plugin = name),
                    detail: tr!(
                        "Its commands and graphs will be removed, along with anything it saved between runs. If you install it again, Clew will ask you again."
                    )
                    .to_string(),
                    yes: tr!("Remove").to_string(),
                    destructive: true,
                    then: Then::Remove(id),
                });
                Task::none()
            }
            PluginMsg::AskRemoveOwnRust => {
                self.plugins.confirm = Some(Confirm {
                    title: tr!("Remove Clew's own Rust?").to_string(),
                    detail: tr!(
                        "Plugins already installed keep working. Building a new one needs Rust again."
                    )
                    .to_string(),
                    yes: tr!("Remove").to_string(),
                    destructive: true,
                    then: Then::RemoveOwnRust,
                });
                Task::none()
            }
            PluginMsg::Confirmed => {
                let Some(confirm) = self.plugins.confirm.take() else {
                    return Task::none();
                };
                match confirm.then {
                    Then::OpenLink(url) => {
                        if let Err(why) = crate::app::content::open_in_browser(&url) {
                            self.status = why;
                        }
                        Task::none()
                    }
                    Then::Remove(id) => self.update_plugins(PluginMsg::Remove(id)),
                    Then::RemoveOwnRust => self.update_plugins(PluginMsg::RemoveOwnRust),
                }
            }
            PluginMsg::ConfirmDismissed => {
                self.plugins.confirm = None;
                Task::none()
            }
            PluginMsg::ToggleLog(id) => {
                if !self.plugins.log_open.remove(&id) {
                    self.plugins.log_open.insert(id);
                }
                Task::none()
            }
            PluginMsg::CopyLog(id) => {
                let lines = self.plugins.log_text(&id);
                if lines.is_empty() {
                    return Task::none();
                }
                // Which plugin, and which build: pasted to an agent, the
                // bare lines did not say what they were the log of.
                let named = match self.plugins.get(&id) {
                    Some(p) => format!(
                        "{} ({id}) {}:\n{lines}",
                        p.name(),
                        p.manifest.plugin.version
                    ),
                    None => lines,
                };
                self.status = tr!("Copied its log").into();
                iced::clipboard::write(named)
            }
            PluginMsg::AskToFix(id) => {
                let Some(plugin) = self.plugins.get(&id) else {
                    return Task::none();
                };
                // In the reader's words, as if typed: they read it, and
                // send it, or not (an answer is paid for).
                let mut ask = tr!(
                    "The plugin {plugin} (id {id}) does not work. Find out why from its log and its source, fix it, build it and try it, then offer me the fixed build to install.",
                    plugin = plugin.name(),
                    id = id.clone()
                );
                let log = self.plugins.log_text(&id);
                if !log.is_empty() {
                    ask.push_str("\n\n");
                    ask.push_str(tr!("Its log:"));
                    ask.push_str("\n```\n");
                    ask.push_str(&log);
                    ask.push_str("\n```");
                }
                let draft = self.proj.ask_input.trim_end();
                self.proj.ask_input = match draft.is_empty() {
                    true => ask,
                    false => format!("{draft}\n\n{ask}"),
                };
                self.proj.ask_spoken = false;
                let closed = self.close_settings();
                // Asked from Settings opened by a graph's See its log, Ask
                // opened under Project Graphs, and nothing showed it had.
                if self.proj.overlay.is_some() {
                    let _ = self.update(Message::Graph(crate::GraphMsg::CloseOverlay));
                }
                Task::batch([closed, self.update(Message::Ask(AskMsg::Open))])
            }
            PluginMsg::Remove(id) => {
                let removed = data().and_then(|d| {
                    clew_plugin::store::remove(&d, &id)?;
                    forget_store_of(&d, &id);
                    Ok(())
                });
                match removed {
                    // Still installed: what is said of it, its log and its
                    // page stand, as it does.
                    Err(why) => self.plugins.failure = Some(why),
                    Ok(()) => {
                        store_changed();
                        self.forget_copy_of(&id, true);
                    }
                }
                self.reload_plugins()
            }
            PluginMsg::Requests(found) => self.on_requests(&found),
            PluginMsg::TakenChanged => {
                self.drop_withdrawn_installs();
                Task::none()
            }
            PluginMsg::RequestRead {
                nonce,
                request,
                built,
            } => match built.lock().ok().and_then(|mut b| b.take()) {
                Some(built) => self.on_request_read(nonce, &request, built),
                None => Task::none(),
            },
            PluginMsg::ToolAnswered {
                stream,
                call,
                tool,
                plugin,
                answer,
            } => {
                let Some(step) = answer.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    return Task::none();
                };
                let reply = Reply::AskTool { stream, call };
                // A try or an install for a conversation put away meanwhile
                // (another project opened): tried, the plugin read the
                // project opened since.
                let current = self.proj.ask_turns.iter().any(|t| t.stream == stream);
                // Or the turn ended — the user's Stop — while it was read:
                // its card came after Stop had taken the turn's cards away,
                // and a try ran the plugin with no one waiting for it.
                let live = self.plugins.tool_cancels.contains_key(&stream);
                let acts = !matches!(step, clew_plugin::tools::Step::Done(_));
                let gone = !(current && live);
                // A plain answer for a turn that is over: its step stays as
                // Stop ended it ("Tried a plugin ✓" after a stopped try).
                if !acts && !live {
                    return Task::none();
                }
                if acts && gone {
                    let content = match current {
                        false => "This conversation's project is no longer open in Clew.",
                        true => {
                            "This turn ended before the tool was answered; nothing was run or offered."
                        }
                    };
                    let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                        stream,
                        call,
                        content: content.into(),
                        is_error: true,
                    });
                    return Task::none();
                }
                match step {
                    clew_plugin::tools::Step::Done(result) => {
                        let (content, is_error) = match result {
                            Ok(text) => (text, false),
                            Err(why) => (why, true),
                        };
                        if tool == "plugin_install" && !is_error {
                            // Answered with no card: the very build is
                            // installed already. "Installed a plugin"
                            // said it was installed now — and of one the
                            // user turned off, nothing said it stays off.
                            let off = plugin
                                .as_deref()
                                .and_then(|id| self.plugins.get(id))
                                .is_some_and(|p| !p.record.enabled);
                            let title = match off {
                                true => tr!("The plugin is already installed, and turned off"),
                                false => tr!("The plugin is already installed"),
                            };
                            self.set_plugin_step(
                                stream,
                                call,
                                crate::app::model::StepState::Done,
                                title.to_string(),
                            );
                        } else {
                            self.finish_plugin_step(
                                stream,
                                call,
                                &tool,
                                if is_error {
                                    crate::app::model::StepState::Failed
                                } else {
                                    crate::app::model::StepState::Done
                                },
                            );
                        }
                        // A build that failed may have failed for want of
                        // Rust: looked at, and said with Settings if so.
                        let check = match (tool.as_str(), is_error) {
                            ("plugin_build", true) => {
                                self.plugins.say_missing_rust = true;
                                self.check_toolchain()
                            }
                            // Built: "plugins cannot be built here" is not
                            // so any more (Rust set up outside Clew since),
                            // and Settings, if it said Rust was missing,
                            // looks again.
                            ("plugin_build", false) => {
                                self.forget_failure_of(FailureOf::PluginBuild);
                                match self.plugins.building {
                                    Building::Missing(_) => self.check_toolchain(),
                                    _ => Task::none(),
                                }
                            }
                            _ => Task::none(),
                        };
                        let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                            stream,
                            call,
                            content,
                            is_error,
                        });
                        // A build or a write may have changed what is
                        // listed; an install changes it only once allowed.
                        check
                    }
                    clew_plugin::tools::Step::Try {
                        built,
                        command,
                        graph,
                        file,
                        line,
                        input,
                    } => self.try_for_agent(
                        *built,
                        Trial {
                            command,
                            graph,
                            file,
                            line,
                            input,
                        },
                        reply,
                    ),
                    clew_plugin::tools::Step::Install { built } => {
                        // Waiting for the user, at the card.
                        self.finish_plugin_step(
                            stream,
                            call,
                            &tool,
                            crate::app::model::StepState::Waiting,
                        );
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
            PluginMsg::GraphBuilt {
                map,
                rev,
                fingerprint,
                serial,
                ran,
                ..
            } => {
                let Some(ran) = ran.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    return Task::none();
                };
                let read = list_read_again(&self.plugins, &ran.outcome);
                let built = self.on_plugin_graph_built(map, rev, fingerprint, serial, ran);
                match read {
                    true => Task::batch([built, self.reload_plugins()]),
                    false => built,
                }
            }
            PluginMsg::RebuildGraph => {
                // What the last build gave, forgotten: the graph shown is
                // asked for again.
                if let Some((id, ..)) = self.shown_plugin_graph()
                    && let Some(entry) = self.proj.plugin_maps.get_mut(&id)
                    && !entry.building
                {
                    entry.failed = None;
                    entry.rev = u64::MAX;
                }
                self.ensure_plugin_map()
            }
            PluginMsg::ForgetHiddenGraph => {
                // What is shown stays: the first graph, picked now.
                self.proj.maps_selected = self
                    .selected_map_index()
                    .and_then(|at| self.proj.maps.get(at))
                    .map(|m| m.id.clone());
                Task::none()
            }
            PluginMsg::CheckToolchain => self.check_toolchain(),
            PluginMsg::ToolchainChecked(checked) => {
                if matches!(self.plugins.building, Building::SettingUp { .. }) {
                    return Task::none();
                }
                // A check that ended while Clew's own Rust was being
                // removed: the removal's own result follows.
                if matches!(self.plugins.building, Building::Removing) && !checked.removal {
                    return Task::none();
                }
                // What it is is known when the other window is done; an
                // agent's failed build is said then, if Rust is missing.
                if checked.elsewhere {
                    // A removal asked for here did not happen: said so, as
                    // the row shows only what the other window does.
                    if checked.removal {
                        self.status = tr!(
                            "Clew's own Rust could not be removed: {reason}",
                            reason = tr!("another Clew window is setting up Rust or removing it")
                                .to_string()
                        );
                    }
                    return self.check_toolchain_when_free();
                }
                // Removed: said, as its 700 MB went with no word; and the
                // other windows look again.
                if matches!(self.plugins.building, Building::Removing) {
                    toolchain_changed();
                    match &checked.removal_failed {
                        // Said where its success is said: Settings may be
                        // closed by then, its row the only other word.
                        Some(why) => {
                            self.status = tr!(
                                "Clew's own Rust could not be removed: {reason}",
                                reason = why.clone()
                            );
                        }
                        None if !checked.own_installed => {
                            self.status = tr!("Clew's own Rust was removed").into();
                        }
                        None => {}
                    }
                }
                self.plugins.own_rust = checked.own_installed;
                // Why it could not be removed holds while it is there.
                self.plugins.removal_failure = checked.removal_failed.or_else(|| {
                    self.plugins
                        .removal_failure
                        .take()
                        .filter(|_| checked.own_installed)
                });
                let say_missing = std::mem::take(&mut self.plugins.say_missing_rust);
                self.plugins.building = match checked.found {
                    Ok((version, own)) => {
                        self.plugins.setup_failure = None;
                        // Rust is ready: "cannot be built here" is not so.
                        self.forget_failure_of(FailureOf::PluginBuild);
                        Building::Ready { version, own }
                    }
                    Err(missing) => {
                        // The agent's build failed for want of Rust: what
                        // sets it up is in Settings → Plugins, said here
                        // with a way there (the agent relays it, perhaps
                        // in words the user does not see).
                        if say_missing {
                            let why = missing.why.clone();
                            self.say_failure_again(
                                move || {
                                    tr!(
                                        "Plugins cannot be built here: {reason}",
                                        reason = crate::ui::missing_words(&why)
                                    )
                                },
                                None,
                                FailureOf::PluginBuild,
                            );
                            self.failure_in(None, Some(SettingsSpot::Rust));
                        }
                        Building::Missing(missing)
                    }
                };
                Task::none()
            }
            PluginMsg::SetUpToolchain => self.set_up_toolchain(),
            PluginMsg::ToolchainStage(stage) => {
                if let Building::SettingUp { stage: now, .. } = &mut self.plugins.building {
                    *now = stage;
                }
                Task::none()
            }
            PluginMsg::ToolchainSetUp(done) => {
                if !matches!(self.plugins.building, Building::SettingUp { .. }) {
                    return Task::none();
                }
                // What there is now, looked at again, here and in the other
                // windows; with why the step did not work if it did not.
                self.plugins.building = Building::Unknown;
                toolchain_changed();
                match done {
                    Ok(version) => {
                        self.plugins.setup_failure = None;
                        self.status = tr!(
                            "Plugins can be built now, with Rust {version}. If an agent was building one, ask it to build again.",
                            version = version
                        );
                    }
                    Err(None) => {
                        self.plugins.setup_failure = None;
                        self.status = tr!("Setting up Rust was cancelled").into();
                    }
                    // Another window got there first: nothing failed, and
                    // the row says so until it is done.
                    Err(Some(clew_plugin::toolchain::SetUpError::Elsewhere)) => {
                        self.plugins.setup_failure = None;
                        return self.check_toolchain_when_free();
                    }
                    // Said on the status bar too: an install takes minutes,
                    // and Settings may be closed by its end.
                    Err(Some(why)) => {
                        // In the reader's words: why plugins still cannot
                        // be built, or Clew's own reason; rustup's own
                        // words as it wrote them.
                        let words = setup_error_words(&why);
                        // It stands, with Settings, where Rust is: said on
                        // the status line it was gone in seconds.
                        self.say_failure_again(
                            move || {
                                tr!(
                                    "Setting up Rust did not work: {reason}",
                                    reason = setup_error_words(&why)
                                )
                            },
                            None,
                            FailureOf::PluginBuild,
                        );
                        self.failure_in(None, Some(SettingsSpot::Rust));
                        self.plugins.setup_failure = Some(words);
                    }
                }
                self.check_toolchain()
            }
            PluginMsg::CancelToolchainSetup => {
                self.cancel_toolchain_setup();
                Task::none()
            }
            PluginMsg::RemoveOwnRust => {
                // Confirmed in a card that stayed up while Rust began to
                // be set up, or another window began to change it: said,
                // not dropped with no word. (One removal at a time: the
                // one going says how it went.)
                let busy = match self.plugins.building {
                    Building::Removing => return Task::none(),
                    Building::SettingUp { .. } => {
                        Some(tr!("Rust is being set up right now").to_string())
                    }
                    Building::Elsewhere => Some(
                        tr!("another Clew window is setting up Rust or removing it").to_string(),
                    ),
                    _ => None,
                };
                let data = match (busy, data()) {
                    (None, Ok(data)) => data,
                    (Some(why), _) | (None, Err(why)) => {
                        self.status = tr!(
                            "Clew's own Rust could not be removed: {reason}",
                            reason = why
                        );
                        return Task::none();
                    }
                };
                self.plugins.building = Building::Removing;
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            // Not while another window sets Rust up or
                            // removes it: its row says so, and updates.
                            let Some(_held) = clew_plugin::toolchain::hold(&data) else {
                                return Checked {
                                    removal: true,
                                    ..Checked::elsewhere()
                                };
                            };
                            let dir = clew_plugin::toolchain::managed_dir(&data);
                            let removed = std::fs::remove_dir_all(&dir);
                            // Still there: why is said by its row, not
                            // dropped with the row left as it was.
                            let removal_failed = removed
                                .err()
                                .filter(|_| dir.exists())
                                .map(|e| e.to_string());
                            Checked {
                                found: clew_plugin::toolchain::find(Some(&data))
                                    .map(|t| (t.version, t.cargo_home.is_some())),
                                own_installed: removal_failed.is_some(),
                                removal_failed,
                                elsewhere: false,
                                removal: true,
                            }
                        })
                        .await
                        .unwrap_or_else(|_| Checked {
                            own_installed: true,
                            removal_failed: Some(tr!("the removal failed unexpectedly").into()),
                            removal: true,
                            ..Checked::failed()
                        })
                    },
                    |checked| Message::Plugin(PluginMsg::ToolchainChecked(checked)),
                )
            }
            PluginMsg::Tried { reply, answer } => {
                match &reply {
                    // Its turn stopped meanwhile: its step stays as Stop
                    // ended it, and nothing is sent to a turn that is over.
                    Reply::AskTool { stream, .. }
                        if !self.plugins.tool_cancels.contains_key(stream) => {}
                    Reply::AskTool { stream, call } => {
                        let (content, is_error) = match answer {
                            Ok(text) => (text, false),
                            Err(why) => (why, true),
                        };
                        self.finish_plugin_step(
                            *stream,
                            *call,
                            "plugin_try",
                            if is_error {
                                crate::app::model::StepState::Failed
                            } else {
                                crate::app::model::StepState::Done
                            },
                        );
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

/// Whether `a` and `b` name the same folder: the server names it as it was
/// resolved (`/private/var/…` for `/var/…`, no `..`, no trailing `/`), the
/// window as it was opened.
fn same_folder(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
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
    clew_plugin::tools::tool_defs(clew_plugin::tools::Caller::Ask)
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

/// The requests of `clew-server mcp` waiting now, looked at once.
pub(crate) fn pending_requests() -> Task<Message> {
    let Ok(data) = data() else {
        return Task::none();
    };
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || clew_plugin::requests::pending(&data))
                .await
                .unwrap_or_default()
        },
        |found| {
            if found.is_empty() {
                Message::Noop
            } else {
                Message::Plugin(PluginMsg::Requests(Arc::new(found)))
            }
        },
    )
}

// `Subscription::run_with` hands its data over as it was given: a `&PathBuf`.
#[allow(clippy::ptr_arg)]
fn watch_requests(data: &PathBuf) -> impl iced::futures::Stream<Item = Message> + use<> {
    let data = data.clone();
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            use iced::futures::SinkExt;
            let mut seen: Vec<String> = Vec::new();
            let mut seen_taken: Vec<String> = Vec::new();
            loop {
                tokio::time::sleep(REQUESTS_EVERY).await;
                let looking = data.clone();
                let (found, taken) = tokio::task::spawn_blocking(move || {
                    (
                        clew_plugin::requests::pending(&looking),
                        clew_plugin::requests::taken(&looking),
                    )
                })
                .await
                .unwrap_or_default();
                // One no longer taken on was answered or withdrawn: the
                // window takes away a card still asking about it.
                if taken != seen_taken {
                    let gone = seen_taken.iter().any(|n| !taken.contains(n));
                    seen_taken = taken;
                    if gone
                        && output
                            .send(Message::Plugin(PluginMsg::TakenChanged))
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
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
