//! Setting a project up: what its first open asks — where clew keeps its
//! data, its language servers, explaining it — and what "Set up project…"
//! and Explain All ask again (see [`ProjectSetup`]).
//!
//! The free, local work (the scan, the symbol index, the graphs) starts on
//! its own. What downloads, runs the project's code or costs money waits for
//! this one sheet, asked once, instead of questions that came one by one
//! whenever a file of a new language happened to open.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Task;

use clew_core::{fs_scan, highlight, lsp, outline};

use crate::app::message::{Message, ProjectMsg};
use crate::app::model::LspSlot;
use crate::app::state::App;
use crate::{tr, trn};

/// What the sheet is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    /// A first open: where clew keeps its data, then what to set up. Not
    /// answering it opens nothing.
    Open,
    /// "Set up project…": what to set up now.
    Manage,
    /// Explain All, asked first: what it costs.
    Explain,
}

/// What setting a language server up takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// Nothing: installed, or on the PATH.
    Nothing,
    /// A download of a verified binary.
    Download,
    /// An install with a tool of the machine's (`go`, `npm`, `cargo`).
    Install(String),
    /// Nothing fetched: the server's own program, already on this
    /// computer, chosen for the project in place of Clew's copy — as a
    /// first open offers it ([`crate::LspMsg::FoundOnPath`]). The tour said
    /// "pyright: install with npm" with pyright on the PATH.
    OnPath(PathBuf),
}

/// A language server the project needs, as the sheet lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupServer {
    pub language: String,
    pub name: String,
    pub fetch: Fetch,
    /// It builds the project to understand it: it runs the project's own
    /// code.
    pub builds: bool,
}

impl SetupServer {
    /// Whether setting it up asks anything: a download, an install or the
    /// leave to build.
    pub fn asks(&self) -> bool {
        self.fetch != Fetch::Nothing || self.builds
    }
}

/// What a look at the project found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Census {
    /// The servers that take something to set up (see [`SetupServer::asks`]).
    pub servers: Vec<SetupServer>,
    /// About how many AI calls explaining what is not explained yet takes;
    /// `None` where it cannot be told (a project on another machine).
    pub calls: Option<usize>,
}

/// The sheet on screen: what it is for, for which project, what is ticked,
/// and what the look at the project found (`None` while it looks).
#[derive(Debug, Clone)]
pub struct ProjectSetup {
    pub mode: SetupMode,
    pub root: PathBuf,
    pub servers: bool,
    pub explain: bool,
    pub census: Option<Census>,
}

/// What a first open's answer asked for, carried out once the project is
/// open and its language choices and saved state have loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupIntent {
    pub root: PathBuf,
    /// The languages whose servers to set up now, and how.
    pub fetch: Vec<(String, Fetch)>,
    pub explain: bool,
}

/// Most files the estimate reads; past it, the rest are taken to be like
/// the ones read.
const SAMPLE_FILES: usize = 400;
/// A file larger than this is not read for the estimate.
const SAMPLE_MAX_BYTES: u64 = 512 * 1024;

/// About how many AI calls explaining `files` takes: one for each function,
/// each file with something to explain and each folder holding one (the
/// pass's own units), read from an even sample of the code files and
/// scaled to all of them.
pub(crate) fn estimate_calls(files: &[fs_scan::FileEntry]) -> usize {
    let code: Vec<(&fs_scan::FileEntry, &'static str)> = files
        .iter()
        .filter_map(|f| highlight::detect(&f.abs).map(|lang| (f, lang)))
        .collect();
    if code.is_empty() {
        return 0;
    }
    let step = code.len().div_ceil(SAMPLE_FILES);
    let (mut read, mut functions, mut with_content) = (0usize, 0usize, 0usize);
    let mut folders = std::collections::HashSet::new();
    for (i, (file, lang)) in code.iter().enumerate() {
        let in_sample = i % step == 0;
        if !in_sample {
            continue;
        }
        let small = std::fs::metadata(&file.abs).is_ok_and(|m| m.len() <= SAMPLE_MAX_BYTES);
        let Some(source) = small
            .then(|| std::fs::read_to_string(&file.abs).ok())
            .flatten()
        else {
            continue;
        };
        read += 1;
        let symbols = outline::extract(&source, lang);
        let callables = symbols
            .iter()
            .filter(|s| outline::is_callable(&s.kind))
            .count();
        functions += callables;
        if !symbols.is_empty() {
            with_content += 1;
            if let Some(parent) = Path::new(&file.rel).parent() {
                folders.insert(parent.to_path_buf());
            }
        }
    }
    if read == 0 {
        return 0;
    }
    let scale = code.len() as f64 / read as f64;
    // Folders are not sampled away: every folder holding a sampled file
    // with content counts once, and those the sample skipped are guessed
    // at the same rate.
    let folders = (folders.len() as f64 * scale.sqrt()).round() as usize;
    ((functions + with_content) as f64 * scale).round() as usize + folders
}

/// What decides the servers a project runs and what each still takes to
/// set up: the project's `lsp.toml` and the user's choices for it (clew's
/// own defaults on a first open, before either is read), and the leave to
/// build each server has on record for it. A copy, so a look runs off the
/// UI thread.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerLook {
    pub config: lsp::config::ProjectLspConfig,
    pub choices: clew_protocol::LanguageChoices,
    pub trust: clew_core::trust::Trust,
    /// Where a server's own program is looked for: the PATH children get.
    /// (Set here for a test, whose PATH is its thread's: the look runs on
    /// another.)
    pub path: Option<std::ffi::OsString>,
}

impl ServerLook {
    /// The language servers `languages` need in the project at `root` that
    /// take something to set up: a download, an install, or the leave to
    /// build that is not on record yet. Blocking: looks for each on disk.
    pub(crate) fn servers_for(&self, root: &Path, languages: &[String]) -> Vec<SetupServer> {
        languages
            .iter()
            .filter_map(|language| {
                let server = self.config.resolve_with(language, &self.choices)?;
                let fetch = match lsp::store::locate(&server) {
                    lsp::store::Located::NeedsDownload { download, .. } => self
                        .on_path(&server.server_name, download.binary)
                        .map_or(Fetch::Download, Fetch::OnPath),
                    lsp::store::Located::NeedsInstall { install, .. } => self
                        .on_path(&server.server_name, install.binary)
                        .map_or_else(|| Fetch::Install(install.tool.to_string()), Fetch::OnPath),
                    lsp::store::Located::Unsupported(_) => return None,
                    lsp::store::Located::Ready(_) | lsp::store::Located::RepoCommand(_) => {
                        Fetch::Nothing
                    }
                };
                // Let build already: nothing to ask.
                let builds = server.runs_project_code().is_some()
                    && !self.trust.is_build_trusted(None, root, &server.server_name);
                Some(SetupServer {
                    language: language.clone(),
                    name: server.server_name,
                    fetch,
                    builds,
                })
            })
            .filter(SetupServer::asks)
            .collect()
    }

    /// The server `server_name`'s own program — `binary` names where it
    /// lands in Clew's copy, its file name what it goes by on the PATH —
    /// when this computer has one worth using, as a first open would offer
    /// it. Not for a server Clew's own install keeps from running the
    /// project's code ([`lsp::langenv::pins`]).
    fn on_path(&self, server_name: &str, binary: &str) -> Option<PathBuf> {
        if lsp::langenv::pins(server_name) {
            return None;
        }
        let name = Path::new(binary).file_name()?.to_str()?;
        let path = self
            .path
            .clone()
            .unwrap_or_else(clew_core::linerpc::child_path);
        let program = lsp::store::find_in_unresolved(&path, name)?;
        crate::app::lsp::worth_offering(&program, &path).then_some(program)
    }
}

/// Folders whose files are not the project's own code: others' code kept in
/// its tree, and the inputs of its tests.
const NOT_ITS_OWN_CODE: &[&str] = &[
    "vendor",
    "third_party",
    "third-party",
    "3rdparty",
    "fixture",
    "fixtures",
    "__fixtures__",
    "testdata",
    "test_data",
    "test-data",
    "__snapshots__",
];

/// Data formats: their servers are set up when one of their files is
/// opened, not asked about for the project.
const DATA_LANGUAGES: &[&str] = &["json", "toml"];

/// A language with at least this many files of the project's own code is
/// one it is written in, whatever its share.
const LANGUAGE_FILES: usize = 5;

/// The languages the project is written in, whose servers setting it up
/// offers (the sheet, the status bar's Set up): served by clew, in the
/// project's own code — not a test's fixture or a vendored library — and
/// more than a file or two of it, as a share. One `.php` among a Rust
/// project's fixtures is not a reason to install a PHP server: opening it
/// asks for one then.
pub(crate) fn setup_languages(files: &[fs_scan::FileEntry]) -> Vec<String> {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for f in files {
        let theirs = f
            .rel
            .split(['/', '\\'])
            .rev()
            .skip(1)
            .any(|dir| NOT_ITS_OWN_CODE.contains(&dir));
        if theirs {
            continue;
        }
        let Some(language) = highlight::detect(&f.abs) else {
            continue;
        };
        if DATA_LANGUAGES.contains(&language)
            || lsp::registry::default_for_language(language).is_none()
        {
            continue;
        }
        *counts.entry(language.to_string()).or_default() += 1;
    }
    let total: usize = counts.values().sum();
    counts
        .into_iter()
        // At least 1 in 20 of the code's files, or a few of them.
        .filter(|(_, n)| *n >= LANGUAGE_FILES || n * 20 >= total)
        .map(|(language, _)| language)
        .collect()
}

/// A look at the project at `root` — its files listed, or `files` when they
/// already are: the servers it needs and about what explaining it costs.
/// Blocking: reads files.
pub(crate) fn census(
    root: &Path,
    files: Option<Arc<Vec<fs_scan::FileEntry>>>,
    look: &ServerLook,
) -> Census {
    let files = files.unwrap_or_else(|| Arc::new(fs_scan::scan(root.to_path_buf()).files));
    Census {
        servers: look.servers_for(root, &setup_languages(&files)),
        calls: Some(estimate_calls(&files)),
    }
}

/// How the sheet names the servers it would set up: each, with what it
/// takes — "rust-analyzer: download, builds the project" — in a list as the
/// reader's language writes one ("a, b", 「a、b」).
pub(crate) fn servers_line(servers: &[SetupServer]) -> String {
    let servers = servers
        .iter()
        .map(|s| {
            let mut takes: Vec<String> = Vec::new();
            match &s.fetch {
                Fetch::Nothing => {}
                Fetch::Download => takes.push(tr!("download").into()),
                Fetch::Install(tool) => takes.push(tr!("install with {tool}", tool = tool)),
                Fetch::OnPath(_) => takes.push(tr!("the one on this computer").into()),
            }
            if s.builds {
                takes.push(tr!("builds the project (runs its code)").into());
            }
            if takes.is_empty() {
                s.name.clone()
            } else {
                tr!("{server}: {needs}", server = s.name, needs = listed(takes))
            }
        })
        .collect::<Vec<_>>();
    listed(servers)
}

/// Items in a list as the reader's language writes one: "a, b, c" in
/// English, 「a、b、c」 in Chinese and Japanese.
fn listed(items: Vec<String>) -> String {
    items
        .into_iter()
        .reduce(|list, item| tr!("{list}, {item}", list = list, item = item))
        .unwrap_or_default()
}

/// `n` as a count said "about": two significant figures past a hundred,
/// grouped by thousands — 3,600, not 3,617.
pub(crate) fn about(n: usize) -> String {
    let rounded = if n < 100 {
        n
    } else {
        let unit = 10usize.pow((n as f64).log10().floor() as u32 - 1);
        (n + unit / 2) / unit * unit
    };
    crate::i18n::number(rounded as u64)
}

/// What explaining costs, as the sheet says it: `calls` from the look at
/// the project (`None` while it looks, `Some(None)` where it cannot be
/// told), and whether the search index is built after.
pub(crate) fn explain_line(llm: bool, calls: Option<Option<usize>>, index: bool) -> String {
    match calls {
        _ if !llm => tr!("Needs an API key in Settings.").into(),
        None => tr!("Counting…").into(),
        Some(Some(0)) => tr!("Nothing left to explain.").into(),
        Some(Some(n)) if index => trn!(
            "About {count} AI call. Also builds the semantic index.",
            "About {count} AI calls. Also builds the semantic index.",
            n,
            count = about(n)
        ),
        Some(Some(n)) => trn!(
            "About {count} AI call.",
            "About {count} AI calls.",
            n,
            count = about(n)
        ),
        Some(None) if index => tr!("Paid AI calls. Also builds the semantic index.").into(),
        Some(None) => tr!("Paid AI calls.").into(),
    }
}

impl App {
    /// Show the sheet, for `mode`, over the project at `root`, and start the
    /// look at it that fills it in.
    pub(crate) fn show_setup(&mut self, mode: SetupMode, root: PathBuf) -> Task<Message> {
        let remote = self.connection.is_remote();
        self.setup = Some(ProjectSetup {
            mode,
            root: root.clone(),
            // Servers are ticked: they cost nothing but the download, and
            // without them go-to-definition is a guess. Explaining costs
            // money, so it waits for a tick — except where the sheet is
            // Explain All asking first.
            servers: true,
            explain: mode == SetupMode::Explain,
            // A project on another machine cannot be looked at from here:
            // its servers run there and ask there, as they need.
            census: remote.then(Census::default),
        });
        if remote {
            return Task::none();
        }
        // An open project's files are listed already; a first open's are
        // read for the look, before anything is written.
        let files = self
            .proj
            .project
            .as_ref()
            .filter(|p| p.root == root)
            .map(|p| p.files.clone());
        let look = self.server_look(&root);
        Task::perform(
            async move {
                let looked = root.clone();
                let census = tokio::task::spawn_blocking(move || census(&looked, files, &look))
                    .await
                    .unwrap_or_default();
                (root, census)
            },
            |(root, census)| Message::Project(ProjectMsg::SetupCounted { root, census }),
        )
    }

    /// The look at the project is in: what is explained already comes off
    /// the estimate.
    pub(crate) fn on_setup_counted(&mut self, root: PathBuf, mut census: Census) -> Task<Message> {
        let explained = if self.proj.project.as_ref().is_some_and(|p| p.root == root) {
            self.proj.explain.cache.len()
        } else {
            0
        };
        census.calls = census.calls.map(|c| c.saturating_sub(explained));
        if let Some(setup) = self.setup.as_mut().filter(|s| s.root == root) {
            // Nothing to set up for the servers: nothing ticked for them.
            if census.servers.is_empty() {
                setup.servers = false;
            }
            setup.census = Some(census);
        }
        Task::none()
    }

    /// The servers the sheet lists, when its servers are ticked.
    fn ticked_servers(setup: &ProjectSetup) -> Vec<SetupServer> {
        if !setup.servers {
            return Vec::new();
        }
        setup
            .census
            .as_ref()
            .map(|c| c.servers.clone())
            .unwrap_or_default()
    }

    /// Record the leave to build for the ticked servers that build the
    /// project — so none of them asks when it starts.
    fn allow_builds(&mut self, root: &Path, servers: &[SetupServer]) {
        let host = self.connection.approval_host().map(str::to_string);
        for server in servers.iter().filter(|s| s.builds) {
            if let Err(e) = self
                .trust
                .update(|t| t.trust_build(host.as_deref(), root, &server.name))
            {
                self.status = tr!("Could not record permission to build: {error}", error = e);
            }
        }
    }

    /// A first open was answered with Open: the leave to build is recorded
    /// now; the downloads and the explaining wait for the project to be open
    /// (see [`App::apply_pending_setup`]).
    pub(crate) fn take_open_setup(&mut self, root: &Path) {
        let Some(setup) = self
            .setup
            .take()
            .filter(|s| s.mode == SetupMode::Open && s.root == root)
        else {
            return;
        };
        let servers = Self::ticked_servers(&setup);
        self.allow_builds(root, &servers);
        let fetch: Vec<(String, Fetch)> = servers
            .iter()
            .filter(|s| s.fetch != Fetch::Nothing)
            .map(|s| (s.language.clone(), s.fetch.clone()))
            .collect();
        let explain = setup.explain && self.llm_available;
        if !fetch.is_empty() || explain {
            self.pending_setup = Some(SetupIntent {
                root: root.to_path_buf(),
                fetch,
                explain,
            });
        }
    }

    /// The sheet's Apply (or Explain): what is ticked, now.
    pub(crate) fn on_setup_apply(&mut self) -> Task<Message> {
        let Some(setup) = self.setup.take() else {
            return Task::none();
        };
        if setup.mode == SetupMode::Open {
            // Its answer is Open (`ProjectMsg::ConsentAllowed`), not this.
            self.setup = Some(setup);
            return Task::none();
        }
        let servers = Self::ticked_servers(&setup);
        let mut tasks = vec![self.set_up_servers(&setup.root, &servers)];
        if setup.explain {
            tasks.push(self.on_explain_project(false));
        }
        Task::batch(tasks)
    }

    /// Set `servers` up for the project at `root`: the leave to build
    /// recorded, and each fetched — or, in hand and declined before,
    /// started afresh.
    fn set_up_servers(&mut self, root: &Path, servers: &[SetupServer]) -> Task<Message> {
        self.allow_builds(root, servers);
        let mut tasks: Vec<Task<Message>> = Vec::new();
        for server in servers {
            if server.fetch != Fetch::Nothing {
                tasks.push(self.set_up_server(&server.language, &server.fetch));
            } else if matches!(
                self.proj.link.lsp.get(&server.language),
                Some(LspSlot::Declined(_))
            ) {
                // In hand, and declined before — the leave to build, just
                // given above: started afresh, as the Languages page's "Ask
                // again" starts it (`LspMsg::DownloadFor`). Left declined,
                // it did not start until the project was opened again.
                self.proj.link.lsp.remove(&server.language);
                tasks.push(self.ensure_lsp(&server.language));
            }
        }
        Task::batch(tasks)
    }

    /// What decides the servers of the project at `root` (see
    /// [`ServerLook`]): the open project's `lsp.toml` and choices when it is
    /// the one open, clew's defaults before it is; the leave to build on
    /// record either way.
    pub(crate) fn server_look(&self, root: &Path) -> ServerLook {
        let open = self.proj.project.as_ref().is_some_and(|p| p.root == root);
        ServerLook {
            config: if open {
                self.proj.lsp_config.clone()
            } else {
                Default::default()
            },
            choices: if open {
                self.proj.lang_choices.clone()
            } else {
                Default::default()
            },
            trust: self.trust.clone(),
            path: crate::app::lsp::path_set_here(),
        }
    }

    /// Look again at what the open project's language servers still take
    /// to set up: what the status bar's Set up does in one click (see
    /// [`App::missing_setup`]). Only once the project's `lsp.toml` and the
    /// user's choices for it are in — their landing looks again — and only
    /// for a project on this machine: a remote one's servers are set up on
    /// its host, which asks there as it needs them.
    pub(crate) fn look_for_missing_setup(&mut self) -> Task<Message> {
        // A look begun before this one saw less: its answer is not taken.
        self.proj.setup_look += 1;
        if self.connection.is_remote() {
            self.proj.setup_missing.clear();
            return Task::none();
        }
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        if self.proj.inflight.state_loading || self.proj.inflight.choices_loading {
            return Task::none();
        }
        let files = self
            .proj
            .project
            .as_ref()
            .map(|p| p.files.clone())
            .unwrap_or_default();
        let look = self.server_look(&root);
        let (stamp, seq) = (self.stamp(), self.proj.setup_look);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    look.servers_for(&root, &setup_languages(&files))
                })
                .await
                .unwrap_or_default()
            },
            move |servers| {
                Message::Project(ProjectMsg::MissingSetupFound {
                    stamp: stamp.clone(),
                    seq,
                    servers,
                })
            },
        )
    }

    /// A look at what the servers still take is in.
    pub(crate) fn on_missing_setup_found(
        &mut self,
        seq: u64,
        servers: Vec<SetupServer>,
    ) -> Task<Message> {
        if seq == self.proj.setup_look {
            self.proj.setup_missing = servers;
        }
        Task::none()
    }

    /// What the status bar offers to set up in one click: what the last
    /// look found, less what has been set up, is on its way or is being
    /// asked about since. A server the user declined, or whose download
    /// failed, is offered again.
    pub(crate) fn missing_setup(&self) -> Vec<&SetupServer> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.as_path()) else {
            return Vec::new();
        };
        if self.connection.is_remote() {
            return Vec::new();
        }
        self.proj
            .setup_missing
            .iter()
            .filter(|s| {
                let open = match self.proj.link.lsp.get(&s.language) {
                    Some(LspSlot::Failed(_) | LspSlot::Declined(_)) | None => true,
                    Some(
                        LspSlot::Ready(_)
                        | LspSlot::Starting
                        | LspSlot::AwaitingConsent
                        | LspSlot::Unsupported(_),
                    ) => false,
                };
                open && (s.fetch != Fetch::Nothing
                    || !self.trust.is_build_trusted(None, root, &s.name))
            })
            .collect()
    }

    /// Whether a one-click Set up is still fetching what it set up.
    pub(crate) fn setting_up(&self) -> bool {
        self.proj
            .setting_up
            .iter()
            .any(|language| self.proj.link.lsp_installs.contains_key(language))
    }

    /// The status bar's Set up: every server [`App::missing_setup`] offers,
    /// set up as the sheet's Apply sets them up.
    pub(crate) fn on_set_up_missing(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let servers: Vec<SetupServer> = self.missing_setup().into_iter().cloned().collect();
        self.proj.setting_up = servers
            .iter()
            .filter(|s| s.fetch != Fetch::Nothing)
            .map(|s| s.language.clone())
            .collect();
        let set_up = self.set_up_servers(&root, &servers);
        // Looked at afresh: a server another window put in place since the
        // last look is found in hand — nothing fetches it, and no fetch's
        // end would take it off the status bar, where Set up stayed.
        Task::batch([set_up, self.look_for_missing_setup()])
    }

    /// Carry out a first open's answer once the project it was for is open
    /// and what its servers and its explaining read has loaded.
    pub(crate) fn apply_pending_setup(&mut self) -> Task<Message> {
        let ready = self.proj.project.as_ref().is_some_and(|p| {
            self.pending_setup
                .as_ref()
                .is_some_and(|intent| intent.root == p.root)
        }) && !self.proj.inflight.state_loading
            && !self.proj.inflight.choices_loading;
        if !ready {
            return Task::none();
        }
        let Some(intent) = self.pending_setup.take() else {
            return Task::none();
        };
        let mut tasks: Vec<Task<Message>> = intent
            .fetch
            .iter()
            .map(|(language, fetch)| self.set_up_server(language, fetch))
            .collect();
        if intent.explain {
            tasks.push(self.on_explain_project(false));
        }
        // The tour's sample, open and set up: its tour begins.
        tasks.push(self.begin_tour_when_open());
        Task::batch(tasks)
    }

    /// Fetch `language`'s server ahead of need — downloaded or installed,
    /// not started: it starts as ever, when a file of the language opens,
    /// at once then. The sheet's tick was the consent the download would
    /// otherwise ask for. A language whose server is in hand, on its way,
    /// running or asked about is left alone; one whose download was declined
    /// or failed is fetched: "Set Up Project…" is where it is asked for again.
    /// Set `language`'s server up as a look at the project found it: the
    /// program on this computer chosen for it, or Clew's copy fetched.
    fn set_up_server(&mut self, language: &str, fetch: &Fetch) -> Task<Message> {
        match fetch {
            Fetch::OnPath(program) => self.use_server_program(language, program),
            _ => self.fetch_lsp(language),
        }
    }

    pub(crate) fn fetch_lsp(&mut self, language: &str) -> Task<Message> {
        if self.connection.is_remote() || self.proj.project.is_none() {
            return Task::none();
        }
        if self
            .proj
            .link
            .lsp
            .get(language)
            .is_some_and(|slot| !matches!(slot, LspSlot::Declined(_) | LspSlot::Failed(_)))
        {
            return Task::none();
        }
        let Some(server) = self.effective_server(language) else {
            return Task::none();
        };
        let (install, dest) = match lsp::store::locate(&server) {
            lsp::store::Located::NeedsDownload { download, dest_dir } => {
                (crate::app::lsp::LocalInstall::Download(download), dest_dir)
            }
            lsp::store::Located::NeedsInstall {
                install, dest_dir, ..
            } => (crate::app::lsp::LocalInstall::Toolchain(install), dest_dir),
            _ => return Task::none(),
        };
        // On its way: a file of the language opened meanwhile waits for it
        // rather than asking for it again.
        self.proj
            .link
            .lsp
            .insert(language.to_string(), LspSlot::Starting);
        self.install_lsp(
            language.to_string(),
            server.server_name,
            server.version,
            install,
            dest,
            false,
        )
    }

    /// A server fetched ahead of need is in (or failed): started now if a
    /// file of its language is open, else left in the store for when one
    /// is.
    pub(crate) fn on_lsp_fetched(
        &mut self,
        language: String,
        result: Result<PathBuf, String>,
    ) -> Task<Message> {
        match result {
            Ok(exe) => {
                // Said done, as a download a file asked for is: the status
                // bar said "Downloading rust-analyzer…" on, beside the
                // server's own "ready".
                self.status = tr!(
                    "{language} server installed",
                    language = crate::app::languages::language_name(&language)
                );
                // Set up — and looked at afresh, so a look begun before it
                // was in, landing after, does not offer it again.
                self.proj.setup_missing.retain(|s| s.language != language);
                let look = self.look_for_missing_setup();
                let wanted = self
                    .proj
                    .panes
                    .iter()
                    .flatten()
                    .any(|v| highlight::detect(&v.abs) == Some(language.as_str()));
                let start = if wanted {
                    self.start_lsp_with(&language, exe)
                } else {
                    self.proj.link.lsp.remove(&language);
                    Task::none()
                };
                Task::batch([start, look])
            }
            Err(e) => {
                self.status = tr!(
                    "Could not download the {language} server: {error}",
                    language = highlight::lang_name(&language).unwrap_or(&language),
                    error = e
                );
                self.proj.link.lsp.insert(language, LspSlot::Failed(e));
                Task::none()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(root: &Path, rel: &str) -> fs_scan::FileEntry {
        fs_scan::FileEntry {
            abs: root.join(rel),
            rel: rel.to_string(),
        }
    }

    /// The estimate counts what a pass explains — functions, files with
    /// something in them, their folders — and nothing for files that are no
    /// code.
    #[test]
    fn the_estimate_counts_functions_files_and_folders() {
        let dir = std::env::temp_dir().join(format!("clew-setup-estimate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/net")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
        std::fs::write(dir.join("src/net/mod.rs"), "fn c() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "# words\n").unwrap();
        std::fs::write(dir.join("notes.bin"), [0u8, 1, 2]).unwrap();
        let files = vec![
            entry(&dir, "src/lib.rs"),
            entry(&dir, "src/net/mod.rs"),
            entry(&dir, "notes.bin"),
        ];
        // Three functions, two files, two folders.
        assert_eq!(estimate_calls(&files), 3 + 2 + 2);
        assert_eq!(estimate_calls(&[]), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Counts are said "about": two figures past a hundred, grouped.
    /// Setting a project up offers the servers of what it is written in:
    /// not a test fixture's language or a vendored library's, not a data
    /// format's, and not a language of a file or two among many.
    #[test]
    fn setup_offers_the_languages_the_project_is_written_in() {
        let root = Path::new("/p");
        let files: Vec<fs_scan::FileEntry> = (0..40)
            .map(|i| entry(root, &format!("src/m{i}.rs")))
            .chain(
                [
                    "tests/fixtures/langs/a.php",
                    "tests/fixtures/langs/a.zig",
                    "vendor/tree-sitter/src/parser.c",
                    "vendor/tree-sitter/src/scanner.c",
                    "third_party/x/y.go",
                    "Cargo.toml",
                    "crates/a/Cargo.toml",
                    "locales/fr.json",
                    "server/debug/dap.erl",
                ]
                .map(|rel| entry(root, rel)),
            )
            .collect();
        assert_eq!(setup_languages(&files), ["rust"]);
        // A few files of a language are a part of the project, whatever its
        // share; and in a small one, a single file can be.
        let mut more = files.clone();
        more.extend((0..5).map(|i| entry(root, &format!("web/p{i}.py"))));
        assert_eq!(setup_languages(&more), ["python", "rust"]);
        let small = [entry(root, "build.zig"), entry(root, "src/main.rs")];
        assert_eq!(setup_languages(&small), ["rust", "zig"]);
        // Each rule on its own, where the share would not decide: a small
        // project's fixture, vendored code and manifest are not languages
        // it is written in.
        let small_with = |rel: &str| [entry(root, "src/main.rs"), entry(root, rel)];
        for theirs in [
            "tests/fixtures/a.php",
            "testdata/b.go",
            "vendor/lib/c.c",
            "Cargo.toml",
            "package.json",
        ] {
            assert_eq!(setup_languages(&small_with(theirs)), ["rust"], "{theirs}");
        }
        // A fixture's folder is only one when it is a folder: a file named
        // so is code.
        assert_eq!(
            setup_languages(&small_with("src/vendor.py")),
            ["python", "rust"]
        );
    }

    #[test]
    fn counts_are_said_about() {
        assert_eq!(about(7), "7");
        assert_eq!(about(99), "99");
        assert_eq!(about(3617), "3,600");
        assert_eq!(about(3650), "3,700");
        assert_eq!(about(128_400), "130,000");
    }

    /// What the sheet says for the servers and the explaining.
    #[test]
    fn the_sheet_says_what_each_choice_takes() {
        let servers = [
            SetupServer {
                language: "rust".into(),
                name: "rust-analyzer".into(),
                fetch: Fetch::Download,
                builds: true,
            },
            SetupServer {
                language: "python".into(),
                name: "pyright".into(),
                fetch: Fetch::Install("npm".into()),
                builds: false,
            },
        ];
        assert_eq!(
            servers_line(&servers),
            "rust-analyzer: download, builds the project (runs its code), pyright: install with npm"
        );
        // As a list is written in the reader's language.
        crate::i18n::with_lang(crate::i18n::Lang::Zh, || {
            assert_eq!(
                servers_line(&servers),
                "rust-analyzer：需下载、会构建项目（运行其代码）、pyright：使用 npm 安装"
            );
        });
        assert_eq!(
            explain_line(true, Some(Some(3617)), true),
            "About 3,600 AI calls. Also builds the semantic index."
        );
        assert_eq!(
            explain_line(true, Some(Some(12)), false),
            "About 12 AI calls."
        );
        assert_eq!(explain_line(true, None, true), "Counting…");
        assert_eq!(
            explain_line(false, Some(Some(5)), true),
            "Needs an API key in Settings."
        );
        assert_eq!(
            explain_line(true, Some(Some(0)), true),
            "Nothing left to explain."
        );
    }

    /// A server that is in hand and builds nothing asks nothing, and is not
    /// listed.
    #[test]
    fn only_servers_that_take_something_are_listed() {
        let ready = SetupServer {
            language: "toml".into(),
            name: "taplo".into(),
            fetch: Fetch::Nothing,
            builds: false,
        };
        assert!(!ready.asks());
        let builds = SetupServer {
            builds: true,
            ..ready.clone()
        };
        assert!(builds.asks());
        let download = SetupServer {
            fetch: Fetch::Download,
            ..ready
        };
        assert!(download.asks());
    }
}
