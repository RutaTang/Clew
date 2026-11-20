//! Running plugins, in Clew's own process, with wasmtime.
//!
//! - One [`Runtime`] per Clew window, made when it first runs a plugin:
//!   one engine, and a thread that ticks its epoch every 10 ms, which is
//!   how a call's deadline is kept.
//! - A plugin is compiled once, and the compiled code cached under
//!   `<data>/plugins/compiled/` by the file's digest and the engine's
//!   settings: a later start is a load of that, under a millisecond.
//! - [`Runtime::load`] links a plugin with only what the user granted it:
//!   an interface it was not granted is not in its linker, so there is no
//!   function to call. `clew_rules::plugin::may_link` decides first.
//! - Every call ([`Loaded::run`], [`Loaded::graph`]) gets a new instance (a
//!   few microseconds, from a pre-linked one): nothing is kept between calls
//!   but what the plugin saves in its own store, a call that crashes takes
//!   nothing with it, and calls run in parallel on any threads.
//! - Each call has a deadline and a memory limit; what stops it is said in
//!   a sentence ([`Failure`]), with the plugin's panic message when it
//!   panicked.
//! - What a plugin returns is checked before anything shows it: lists and
//!   text are capped, and a place outside the project is dropped.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use wasmtime::component::{Component, HasData, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store, Trap};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::bindings::clocks::monotonic_clock;
use wasmtime_wasi::p2::{DynPollable, Pollable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::inspect::{Inspected, hex};
use crate::manifest::permission_names;

#[allow(missing_docs, clippy::all)]
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
        additional_derives: [PartialEq],
    });
}

pub use bindings::clew::plugin::project::ProjectInfo;
pub use bindings::clew::plugin::types::{
    BlameLine, Block, Code, Commit, Fact, Graph, GraphEdge, GraphNode, Hit, Invocation, Item,
    Level, Location, Notice, Outcome, Query, Row, Symbol, Table, TreeRow, View,
};

/// How often the engine's epoch ticks: the grain of a call's deadline.
const TICK: Duration = Duration::from_millis(10);

/// What a plugin may write to its standard output or error in one call;
/// the rest is dropped, never an error (Rust's `println!` panics on one).
const OUTPUT_CAP: usize = 64 * 1024;

/// How much of the project's text one call may read, in all.
const READ_BUDGET: u64 = 1024 * 1024 * 1024;

/// The most a plugin may keep in its own store, and the longest key.
const STORE_CAP: u64 = 1024 * 1024;
const KEY_CAP: usize = clew_rules::plugin::KEY_MAX;

/// What Clew gives a plugin of the open project. The GUI implements it
/// over a local project or a remote one alike; every path it is handed has
/// passed `confine::check_lexical` already, and it confines again as it
/// reads.
pub trait Host: Send + Sync {
    fn info(&self) -> ProjectInfo;
    fn files(&self) -> Vec<String>;
    fn read(&self, path: &str) -> Result<String, String>;
    fn outline(&self, path: &str) -> Result<Vec<Symbol>, String>;
    fn search(&self, query: &Query) -> Result<Vec<Hit>, String>;
    fn git_log(&self, path: Option<&str>, limit: u32) -> Result<Vec<Commit>, String>;
    fn git_blame(&self, path: &str) -> Result<Vec<BlameLine>, String>;
    /// How far a long task has come.
    fn progress(&self, _done: u32, _total: u32) {}
    /// A line of the plugin's log.
    fn log(&self, _level: Level, _message: &str) {}
    /// Whether the user asked the run to stop: it is stopped within a
    /// tick, a wait of the plugin's included ([`STOPPED_BY_USER`]).
    fn stop_asked(&self) -> bool {
        false
    }
    /// When the run must end (its deadline): a host call that waits — on a
    /// remote project, a read over the network — gives up then, and when
    /// [`stop_asked`](Host::stop_asked), rather than holding the run past
    /// both until it is answered.
    fn run_ends(&self, _ends: Instant) {}
}

/// Why a run the user stopped gave nothing back ([`Failure::Stopped`]).
pub const STOPPED_BY_USER: &str = "the user stopped it";

/// How long a call may run, and how much memory it may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub deadline: Duration,
    pub memory: usize,
    /// The most bytes of the plugin's answer Clew copies out of its memory
    /// (wasmtime's hostcall fuel, spent on every string and list read). Its
    /// strings and lists may all point at the same bytes: within its own
    /// memory, a plugin could name 100 MiB a million times over, and Clew
    /// was killed for want of memory copying them before any cap was
    /// applied. Over it, the run fails ([`ANSWER_TOO_LARGE`]).
    pub answer: usize,
}

impl Limits {
    /// For a command.
    pub const COMMAND: Limits = Limits {
        deadline: Duration::from_secs(30),
        memory: 256 * 1024 * 1024,
        answer: ANSWER_BYTES,
    };
    /// For a graph, which reads the whole project.
    pub const GRAPH: Limits = Limits {
        deadline: Duration::from_secs(60),
        memory: 512 * 1024 * 1024,
        answer: ANSWER_BYTES,
    };
}

/// What Clew copies of a plugin's answer at most: sixteen times what a
/// page shows ([`MAX_VIEW_TEXT`]), and a graph's most nodes and edges with
/// room to spare.
const ANSWER_BYTES: usize = 64 * 1024 * 1024;

/// Why a run gave nothing back whose answer was larger than
/// [`Limits::answer`].
pub const ANSWER_TOO_LARGE: &str = "its answer was larger than Clew takes from a plugin";

/// Why a command that asked to open a place outside the project was
/// refused.
pub const OUTSIDE_PLACE: &str = "it asked to open a place outside the project";

/// Why a call gave nothing back. Each case the window says in the
/// reader's words; [`Display`](std::fmt::Display) says it in English, for
/// an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The plugin said it could not, in its own words.
    Plugin(String),
    /// It ran past its time limit, this many seconds, and was stopped.
    TimedOut(u64),
    /// It needed more memory than its limit, this many MB, and was stopped.
    OutOfMemory(u64),
    /// It crashed: the panic's message, or the trap.
    Crashed(String),
    /// It could not be run at all: why, in a sentence.
    Stopped(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Plugin(why) | Failure::Stopped(why) => f.write_str(why),
            Failure::TimedOut(secs) => {
                write!(f, "the plugin took longer than {secs} s and was stopped")
            }
            Failure::OutOfMemory(mb) => {
                write!(
                    f,
                    "the plugin needed more than {mb} MB of memory and was stopped"
                )
            }
            Failure::Crashed(why) => write!(f, "the plugin crashed: {why}"),
        }
    }
}

/// The plugin runtime: one per Clew window, made when it first runs a
/// plugin.
pub struct Runtime {
    engine: Engine,
    data: Option<PathBuf>,
}

impl Runtime {
    /// A runtime keeping compiled plugins and the plugins' own stores under
    /// `data` (Clew's data folder); with none, nothing is kept.
    pub fn new(data: Option<PathBuf>) -> Result<Runtime, String> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        config.max_wasm_stack(1024 * 1024);
        config.parallel_compilation(true);
        let engine = Engine::new(&config).map_err(|e| format!("the plugin engine: {e}"))?;
        start_ticker(&engine);
        Ok(Runtime { engine, data })
    }

    /// The plugin file `bytes`, as [`crate::inspect()`] read it, made
    /// ready to run with the permissions `granted` (`clew_rules::plugin`
    /// bits). Refused when its code needs a permission not granted.
    pub fn load(&self, bytes: &[u8], inspected: &Inspected, granted: u8) -> Result<Loaded, String> {
        self.load_as(bytes, inspected, granted, true)
    }

    /// `bytes` made ready to try, as [`Runtime::load`] does, but as a
    /// plugin not installed: its store is its own, empty, and gone after
    /// (not the store of an installed plugin of the same id), and its
    /// compiled code is not kept.
    pub fn load_trial(
        &self,
        bytes: &[u8],
        inspected: &Inspected,
        granted: u8,
    ) -> Result<Loaded, String> {
        self.load_as(bytes, inspected, granted, false)
    }

    fn load_as(
        &self,
        bytes: &[u8],
        inspected: &Inspected,
        granted: u8,
        installed: bool,
    ) -> Result<Loaded, String> {
        if !clew_rules::plugin::may_link(inspected.needs, granted) {
            let missing = inspected.needs & !granted;
            return Err(format!(
                "the plugin needs a permission it was not granted: {}",
                permission_names(missing).join(", ")
            ));
        }
        let component = if installed {
            self.compile(bytes, &inspected.sha256)?
        } else {
            Component::new(&self.engine, bytes)
                .map_err(|e| format!("could not compile the plugin: {e}"))?
        };
        let mut linker = Linker::<State>::new(&self.engine);
        link(&mut linker, granted).map_err(|e| format!("could not link the plugin: {e}"))?;
        let pre = linker
            .instantiate_pre(&component)
            .map_err(|e| format!("could not link the plugin: {e}"))?;
        let pre = bindings::PluginPre::new(pre)
            .map_err(|e| format!("the plugin does not export what Clew calls: {e}"))?;
        let store = match (&self.data, installed) {
            (Some(data), true) => store_dir(data, &inspected.manifest.plugin.id).ok(),
            (Some(data), false) => Some(trial_dir(data)),
            (None, _) => None,
        };
        Ok(Loaded {
            pre,
            store,
            trial: !installed,
        })
    }

    /// The compiled plugin: loaded from the cache, or compiled and cached.
    fn compile(&self, bytes: &[u8], sha256: &[u8; 32]) -> Result<Component, String> {
        let cached = self.data.as_ref().map(|data| {
            crate::store::plugins_dir(data)
                .join("compiled")
                .join(format!("{}-{}.cwasm", hex(sha256), self.compatibility()))
        });
        if let Some(path) = cached.as_ref().filter(|p| p.is_file()) {
            // SAFETY: `deserialize_file` runs what the file holds as native
            // code, so the file must be wasmtime's own output. It is: Clew
            // wrote it below, with `serialize`, into its own private data
            // folder, under a name that carries the engine's settings; a file
            // there that is not is no less trusted than Clew's own
            // executable beside it. It is always replaced by a rename,
            // never written in place, so a plugin loaded from it keeps the
            // code it mapped.
            match unsafe { Component::deserialize_file(&self.engine, path) } {
                Ok(component) => return Ok(component),
                // Damaged or stale: compiled again and replaced.
                Err(_) => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let component = Component::new(&self.engine, bytes)
            .map_err(|e| format!("could not compile the plugin: {e}"))?;
        if let (Some(path), Ok(serialized)) = (&cached, component.serialize())
            && let Some(dir) = path.parent()
        {
            if let Some(plugins) = dir.parent() {
                let _ = clew_core::derived::ensure_private_dir(plugins);
            }
            if clew_core::derived::ensure_private_dir(dir).is_ok() {
                let _ = clew_core::statefile::write_atomic(path, &serialized);
            }
        }
        Ok(component)
    }

    /// The engine's settings, as a name: compiled code is only loaded by an
    /// engine that made it.
    fn compatibility(&self) -> String {
        let mut h = DefaultHasher::new();
        self.engine.precompile_compatibility_hash().hash(&mut h);
        format!("{:016x}", h.finish())
    }

    /// Remove compiled plugins whose file's digest is not in `keep` (hex).
    pub fn sweep_compiled(&self, keep: &[String]) {
        let Some(data) = &self.data else { return };
        let dir = crate::store::plugins_dir(data).join("compiled");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let current = format!("-{}.cwasm", self.compatibility());
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let kept = name
                .strip_suffix(&current)
                .is_some_and(|sha| keep.iter().any(|k| k == sha));
            if !kept {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Tick `engine`'s epoch until the engine is dropped.
fn start_ticker(engine: &Engine) {
    let weak = engine.weak();
    let _ = std::thread::Builder::new()
        .name("clew-plugin-epoch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(TICK);
                match weak.upgrade() {
                    Some(engine) => engine.increment_epoch(),
                    None => return,
                }
            }
        });
}

/// Add to `linker` what every plugin gets — WASI with nothing behind it,
/// the types, the host — and the interfaces `granted` covers.
fn link(linker: &mut Linker<State>, granted: u8) -> wasmtime::Result<()> {
    wasmtime_wasi::p2::add_to_linker_sync(linker)?;
    // WASI's own clock waits as long as it is asked to, in the host, where
    // the deadline does not reach: a plugin that slept (`thread::sleep`,
    // a poll on a far deadline) was never stopped, and held its run — and
    // every run after it — for good. Its waits end at the deadline.
    linker.allow_shadowing(true);
    monotonic_clock::add_to_linker::<State, RunClock>(linker, |s| ClockView {
        table: &mut s.table,
        clock: s.clock.clone(),
    })?;
    linker.allow_shadowing(false);
    bindings::clew::plugin::types::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    bindings::clew::plugin::host::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    if granted & clew_rules::plugin::READ != 0 {
        bindings::clew::plugin::project::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    }
    if granted & clew_rules::plugin::GIT != 0 {
        bindings::clew::plugin::git::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    }
    Ok(())
}

/// A plugin, linked and ready to run.
pub struct Loaded {
    pre: bindings::PluginPre<State>,
    /// Its own store's folder.
    store: Option<PathBuf>,
    /// Loaded to be tried: its store goes with it.
    trial: bool,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        if self.trial
            && let Some(store) = &self.store
        {
            let _ = std::fs::remove_dir_all(store);
        }
    }
}

/// A store of its own for a plugin being tried, in `<data>/plugins/tries`:
/// a folder no other run names.
fn trial_dir(data: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    crate::store::plugins_dir(data)
        .join("tries")
        .join(format!("{}-{now}-{n}", std::process::id()))
}

impl Loaded {
    /// Run its command `command`.
    pub fn run(
        &self,
        host: Arc<dyn Host>,
        command: &str,
        invocation: &Invocation,
        limits: Limits,
    ) -> Result<Outcome, Failure> {
        let outcome = self.call(host, limits, |plugin, store| {
            plugin
                .clew_plugin_commands()
                .call_run(store, command, invocation)
        })?;
        checked_outcome(outcome)
    }

    /// Build its graph `graph`.
    pub fn graph(
        &self,
        host: Arc<dyn Host>,
        graph: &str,
        limits: Limits,
    ) -> Result<Graph, Failure> {
        let graph = self.call(host, limits, |plugin, store| {
            plugin.clew_plugin_graphs().call_build(store, graph)
        })?;
        Ok(checked_graph(graph))
    }

    /// Make an instance, call `f` on it under `limits`, and say what
    /// stopped it if anything did.
    fn call<R>(
        &self,
        host: Arc<dyn Host>,
        limits: Limits,
        f: impl FnOnce(&bindings::Plugin, &mut Store<State>) -> wasmtime::Result<Result<R, String>>,
    ) -> Result<R, Failure> {
        host.run_ends(Instant::now() + limits.deadline);
        let stdout = Capture::default();
        let stderr = Capture::default();
        let wasi = WasiCtxBuilder::new()
            .stdout(stdout.clone())
            .stderr(stderr.clone())
            .allow_ip_name_lookup(false)
            .allow_tcp(false)
            .allow_udp(false)
            .socket_addr_check(|_, _| Box::pin(async { false }))
            .build();
        let state = State {
            wasi,
            table: ResourceTable::new(),
            limiter: Limiter {
                memory: limits.memory,
                used: 0,
                hit: false,
            },
            host: host.clone(),
            store: self.store.clone(),
            clock: RunClock::until(limits.deadline).asked_by(host.clone()),
            read: 0,
            logged: 0,
            logged_bytes: 0,
            log_dropped: 0,
        };
        let mut store = Store::new(self.pre.engine(), state);
        store.limiter(|s| &mut s.limiter);
        store.set_hostcall_fuel(limits.answer);
        // Looked at every tick: past its deadline, or stopped by the user,
        // the code is interrupted where it is.
        let ends = Instant::now() + limits.deadline;
        let asker = host.clone();
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            Ok(if asker.stop_asked() || Instant::now() >= ends {
                wasmtime::UpdateDeadline::Interrupt
            } else {
                wasmtime::UpdateDeadline::Continue(1)
            })
        });

        let result = self
            .pre
            .instantiate(&mut store)
            .and_then(|plugin| f(&plugin, &mut store));
        forward_output(host.as_ref(), Level::Info, &stdout);
        forward_output(host.as_ref(), Level::Warning, &stderr);
        let dropped = store.data().log_dropped;
        if dropped > 0 {
            host.log(
                Level::Warning,
                &format!("{dropped} more log lines were left out: a run keeps {LOG_LINES_PER_CALL} lines at most"),
            );
        }
        // Stopped by the user: nothing it gave back counts, whatever it was.
        if host.stop_asked() {
            return Err(Failure::Stopped(STOPPED_BY_USER.into()));
        }
        // A wait cut short at the deadline used the run's time up: what the
        // plugin did after it, it did believing it had waited.
        if store.data().clock.ran_out() {
            return Err(Failure::TimedOut(limits.deadline.as_secs()));
        }
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(why)) => Err(Failure::Plugin(capped(&why, 4096))),
            Err(e) => Err(stopped(&e, limits, store.data().limiter.hit, &stderr)),
        }
    }
}

/// A run's `wasi:clocks/monotonic-clock`: time from the run's start, and
/// waits that end at its deadline at the latest.
#[derive(Clone)]
struct RunClock {
    started: Instant,
    ends: Instant,
    /// Set once a wait went on to the deadline.
    out: Arc<AtomicBool>,
    /// Who may ask the run to stop: its waits end when it does.
    asker: Option<Arc<dyn Host>>,
}

impl RunClock {
    fn until(deadline: Duration) -> Self {
        let started = Instant::now();
        RunClock {
            started,
            ends: started + deadline,
            out: Arc::default(),
            asker: None,
        }
    }

    /// Its waits end, too, when `host` asks the run to stop.
    fn asked_by(mut self, host: Arc<dyn Host>) -> Self {
        self.asker = Some(host);
        self
    }

    /// Whether a wait went on to the deadline.
    fn ran_out(&self) -> bool {
        self.out.load(Ordering::Relaxed)
    }
}

impl HasData for RunClock {
    type Data<'a> = ClockView<'a>;
}

/// What the clock's calls need of a run's state.
struct ClockView<'a> {
    table: &'a mut ResourceTable,
    clock: RunClock,
}

impl ClockView<'_> {
    /// A pollable that is ready after `wanted`, or at the deadline if that
    /// comes first.
    fn wait(&mut self, wanted: Duration) -> wasmtime::Result<Resource<DynPollable>> {
        let now = Instant::now();
        let left = self.clock.ends.saturating_duration_since(now);
        let wait = Wait {
            until: (!wanted.is_zero())
                .then(|| tokio::time::Instant::from_std(now + wanted.min(left))),
            yielded: false,
            out: (wanted >= left).then(|| self.clock.out.clone()),
            asker: self.clock.asker.clone(),
        };
        let wait = self.table.push(wait)?;
        wasmtime_wasi::p2::subscribe(self.table, wait)
    }
}

impl monotonic_clock::Host for ClockView<'_> {
    fn now(&mut self) -> wasmtime::Result<monotonic_clock::Instant> {
        Ok(u64::try_from(self.clock.started.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    fn resolution(&mut self) -> wasmtime::Result<monotonic_clock::Duration> {
        Ok(1)
    }

    fn subscribe_instant(
        &mut self,
        when: monotonic_clock::Instant,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        let now = self.now()?;
        self.wait(Duration::from_nanos(when.saturating_sub(now)))
    }

    fn subscribe_duration(
        &mut self,
        duration: monotonic_clock::Duration,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        self.wait(Duration::from_nanos(duration))
    }
}

/// A clock's wait (`RunClock`).
struct Wait {
    /// When it is over; `None` at once.
    until: Option<tokio::time::Instant>,
    /// Ready at once, it gives the runtime one turn first, as WASI's own
    /// does: a plugin polling in a loop does not starve it.
    yielded: bool,
    /// Where to say it went on to the deadline, if it was cut to it.
    out: Option<Arc<AtomicBool>>,
    /// Who may ask the run to stop: the wait ends then.
    asker: Option<Arc<dyn Host>>,
}

/// How often a wait looks whether the user asked its run to stop.
const STOP_LOOK: Duration = Duration::from_millis(50);

#[wasmtime_wasi::async_trait]
impl Pollable for Wait {
    async fn ready(&mut self) {
        match self.until {
            // In steps, looking between them whether the run was stopped:
            // it is, as soon as the plugin runs again.
            Some(until) => loop {
                let step = tokio::time::Instant::now() + STOP_LOOK;
                if step >= until {
                    tokio::time::sleep_until(until).await;
                    break;
                }
                tokio::time::sleep_until(step).await;
                if self.asker.as_ref().is_some_and(|h| h.stop_asked()) {
                    return;
                }
            },
            None if !self.yielded => {
                self.yielded = true;
                tokio::task::yield_now().await;
            }
            None => {}
        }
        if let Some(out) = &self.out {
            out.store(true, Ordering::Relaxed);
        }
    }
}

/// Why a call that trapped stopped, in a sentence.
fn stopped(e: &wasmtime::Error, limits: Limits, out_of_memory: bool, stderr: &Capture) -> Failure {
    if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
        return Failure::TimedOut(limits.deadline.as_secs());
    }
    // wasmtime's own words for an answer past `Limits::answer` (its type is
    // not public).
    if e.chain().any(|c| {
        c.to_string()
            .contains("allocated for hostcalls has been exhausted")
    }) {
        return Failure::Stopped(ANSWER_TOO_LARGE.into());
    }
    if out_of_memory {
        return Failure::OutOfMemory((limits.memory / (1024 * 1024)) as u64);
    }
    if let Some(panic) = panic_message(&stderr.text()) {
        return Failure::Crashed(panic);
    }
    let why = match e.downcast_ref::<Trap>() {
        Some(trap) => trap.to_string(),
        None => e.to_string(),
    };
    Failure::Crashed(capped(&why, 512))
}

/// The message of a Rust panic, as the standard library writes it to
/// stderr: `thread '…' panicked at src/lib.rs:3:5:` and the message after.
fn panic_message(stderr: &str) -> Option<String> {
    let at = stderr.find("panicked at ")?;
    let rest = &stderr[at + "panicked at ".len()..];
    let (place, message) = rest.split_once('\n').unwrap_or((rest, ""));
    let place = place.trim_end_matches(':');
    let message = message
        .lines()
        .take_while(|l| !l.starts_with("note:"))
        .collect::<Vec<_>>()
        .join(" ");
    let message = message.trim();
    Some(if message.is_empty() {
        format!("at {place}")
    } else {
        format!("{} (at {place})", capped(message, 400))
    })
}

/// What of `text` is kept: cut past [`OUTPUT_CAP`], up to its last whole
/// line — or, one line longer than all of it, its start rather than
/// nothing, without a character cut partway.
fn kept_output(text: &str, cut: bool) -> &str {
    match cut {
        true => text
            .rsplit_once('\n')
            .map_or(text.trim_end_matches('\u{FFFD}'), |(whole, _)| whole),
        false => text,
    }
}

/// Write what a call printed to the plugin's log; past [`OUTPUT_CAP`], the
/// line cut partway is left out with the rest, and the log says so — it was
/// cut mid-line with no word.
fn forward_output(host: &dyn Host, level: Level, capture: &Capture) {
    let text = without_backtrace_hint(&capture.text());
    let cut = capture.dropped() > 0;
    let text = kept_output(&text, cut).trim();
    if !text.is_empty() {
        host.log(level, text);
    }
    if cut {
        host.log(
            Level::Warning,
            &format!(
                "what it printed past {} KiB was left out",
                OUTPUT_CAP / 1024
            ),
        );
    }
}

/// `text` without the standard library's hint after a panic: it names an
/// environment variable no one can set for a plugin, and in the log it was
/// a line to read with nothing to do about it.
fn without_backtrace_hint(text: &str) -> String {
    text.lines()
        .filter(|line| !line.starts_with("note: run with `RUST_BACKTRACE=1`"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The most of a plugin's log one call passes on, in lines and in bytes:
/// past them a loop writing to it would fill the window's memory, not
/// the plugin's, which is limited.
const LOG_LINES_PER_CALL: usize = 1000;
const LOG_BYTES_PER_CALL: usize = 256 * 1024;

/// What one call's instance holds.
struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limiter: Limiter,
    host: Arc<dyn Host>,
    /// The plugin's own store's folder.
    store: Option<PathBuf>,
    /// The run's clock: its waits end at the deadline.
    clock: RunClock,
    /// The bytes of the project read so far.
    read: u64,
    /// The log lines passed on so far, their bytes, and those left out.
    logged: usize,
    logged_bytes: usize,
    log_dropped: usize,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// The memory limit, which also records that it refused.
struct Limiter {
    memory: usize,
    used: usize,
    hit: bool,
}

/// What a table's element takes of the host's memory, at the least: a
/// pointer.
const TABLE_ELEMENT_BYTES: usize = 8;

impl Limiter {
    /// Grow what is used by `current` → `desired` bytes, if the budget has
    /// room for it: one budget for every memory and table of the run — a
    /// table counted for nothing let a plugin take gigabytes of the
    /// window's memory in tables of a million elements each.
    fn grow(&mut self, current: usize, desired: usize) -> bool {
        let after = (self.used + desired).saturating_sub(current);
        if after > self.memory {
            self.hit = true;
            return false;
        }
        self.used = after;
        true
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(self.grow(current, desired))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 1_000_000
            && self.grow(
                current.saturating_mul(TABLE_ELEMENT_BYTES),
                desired.saturating_mul(TABLE_ELEMENT_BYTES),
            ))
    }
}

/// A path the plugin named, refused unless it stays inside the project.
fn confined(path: &str) -> Result<(), String> {
    if path.is_empty() || clew_core::confine::check_lexical(path).is_err() {
        return Err(format!(
            "\"{}\" is not a path in the project",
            capped(path, 200)
        ));
    }
    Ok(())
}

impl bindings::clew::plugin::types::Host for State {}

impl bindings::clew::plugin::host::Host for State {
    fn log(&mut self, level: Level, message: String) {
        let message = capped(&message, 4096);
        if self.logged >= LOG_LINES_PER_CALL
            || self.logged_bytes + message.len() > LOG_BYTES_PER_CALL
        {
            self.log_dropped += 1;
            return;
        }
        self.logged += 1;
        self.logged_bytes += message.len();
        self.host.log(level, &message);
    }

    fn progress(&mut self, done: u32, total: u32) {
        self.host.progress(done.min(total), total);
    }

    fn load(&mut self, key: String) -> Option<Vec<u8>> {
        let path = self.store.as_ref()?.join(key_file(&key)?);
        std::fs::read(path).ok()
    }

    fn save(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        let dir = self
            .store
            .clone()
            .ok_or_else(|| "this plugin has no store here".to_string())?;
        let name = key_file(&key).ok_or_else(|| format!("a key is 1 to {KEY_CAP} bytes"))?;
        let others: u64 = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy() != name)
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0);
        if others + value.len() as u64 > STORE_CAP {
            return Err(format!(
                "the plugin's store holds {} KB at most",
                STORE_CAP / 1024
            ));
        }
        if let Some(parent) = dir.parent() {
            clew_core::derived::ensure_private_dir(parent).map_err(|e| e.to_string())?;
        }
        clew_core::derived::ensure_private_dir(&dir).map_err(|e| e.to_string())?;
        clew_core::statefile::write_atomic(&dir.join(name), &value).map_err(|e| e.to_string())
    }
}

/// The file a store key is kept in: its bytes as hex and `.bin`
/// (`clew_rules::plugin::key_name`, proved to be a plain file name no other
/// key shares). `None` for a key too long or empty.
fn key_file(key: &str) -> Option<String> {
    let name = clew_rules::plugin::key_name(key.as_bytes());
    (!name.is_empty()).then(|| String::from_utf8_lossy(&name).into_owned())
}

impl bindings::clew::plugin::project::Host for State {
    fn info(&mut self) -> ProjectInfo {
        self.host.info()
    }

    fn files(&mut self) -> Vec<String> {
        self.host.files()
    }

    fn read(&mut self, path: String) -> Result<String, String> {
        confined(&path)?;
        if self.read > READ_BUDGET {
            return Err(format!(
                "a call may read {} GB of the project at most",
                READ_BUDGET / (1024 * 1024 * 1024)
            ));
        }
        let text = self.host.read(&path)?;
        self.read += text.len() as u64;
        Ok(text)
    }

    fn outline(&mut self, path: String) -> Result<Vec<Symbol>, String> {
        confined(&path)?;
        self.host.outline(&path)
    }

    fn search(&mut self, query: Query) -> Result<Vec<Hit>, String> {
        if query.query.is_empty() {
            return Err("the search is empty".into());
        }
        self.host.search(&query)
    }
}

impl bindings::clew::plugin::git::Host for State {
    fn log(&mut self, path: Option<String>, limit: u32) -> Result<Vec<Commit>, String> {
        if let Some(path) = &path {
            confined(path)?;
        }
        self.host.git_log(path.as_deref(), limit.min(10_000))
    }

    fn blame(&mut self, path: String) -> Result<Vec<BlameLine>, String> {
        confined(&path)?;
        self.host.git_blame(&path)
    }
}

/// Standard output or error, kept up to [`OUTPUT_CAP`] bytes; what comes
/// after is accepted, dropped, and counted.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Captured>>);

#[derive(Default)]
struct Captured {
    kept: Vec<u8>,
    dropped: usize,
}

impl Capture {
    fn text(&self) -> String {
        let captured = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&captured.kept).into_owned()
    }

    /// How many bytes past [`OUTPUT_CAP`] were dropped.
    fn dropped(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }

    /// Keep what fits under [`OUTPUT_CAP`], and count the rest.
    fn take(&self, buf: &[u8]) {
        let mut captured = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let taken = buf
            .len()
            .min(OUTPUT_CAP.saturating_sub(captured.kept.len()));
        captured.kept.extend_from_slice(&buf[..taken]);
        captured.dropped += buf.len() - taken;
    }
}

impl IsTerminal for Capture {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for Capture {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }

    /// Written to in place. The default stream hands each write to a task
    /// of its own and waits for it: a plugin printing 20 000 lines took
    /// 1.5 s in a debug build, most of a 2 s deadline, in thread hops for
    /// what is a copy into memory.
    fn p2_stream(&self) -> Box<dyn wasmtime_wasi::p2::OutputStream> {
        Box::new(self.clone())
    }
}

impl wasmtime_wasi::p2::OutputStream for Capture {
    fn write(&mut self, bytes: bytes::Bytes) -> wasmtime_wasi::p2::StreamResult<()> {
        self.take(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> wasmtime_wasi::p2::StreamResult<()> {
        Ok(())
    }

    /// Always room: past [`OUTPUT_CAP`] what is written is dropped and
    /// counted, never refused (a refusal panics the plugin's `println!`).
    fn check_write(&mut self) -> wasmtime_wasi::p2::StreamResult<usize> {
        Ok(OUTPUT_CAP)
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for Capture {
    async fn ready(&mut self) {}
}

impl tokio::io::AsyncWrite for Capture {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.take(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// -- what a plugin returns, checked ------------------------------------------

/// The most blocks a view may have, and rows in all its lists together.
pub const MAX_BLOCKS: usize = 500;
pub const MAX_ROWS: usize = 10_000;
/// The most text a page holds, in bytes, across all its blocks.
pub const MAX_VIEW_TEXT: usize = 4 * 1024 * 1024;
/// The notice ending a page cut short (the window says it in the reader's
/// language).
pub const CUT_NOTICE: &str =
    "The plugin gave more than Clew shows on a page; the rest is left out.";
const MAX_COLUMNS: usize = 32;
/// The most nodes and edges a graph may have.
pub const MAX_NODES: usize = 4000;
pub const MAX_EDGES: usize = 20_000;
/// The longest a line of a view (a title, a cell) and a block of text may
/// be, in bytes.
const LINE_CAP: usize = 1024;
const TEXT_CAP: usize = 64 * 1024;

/// `text`, cut at a character boundary to `max` bytes at most, with `…`
/// where it was cut.
fn capped(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// A line: capped, and its line breaks turned to spaces.
fn line(text: &str) -> String {
    capped(&text.replace(['\r', '\n'], " "), LINE_CAP)
}

/// A place, if it is one in the project.
fn place(at: Option<Location>) -> Option<Location> {
    at.filter(|l| confined(&l.path).is_ok())
}

/// What a command left, checked: capped, and with its places in the
/// project. A command that says to open a place outside it is refused.
pub fn checked_outcome(outcome: Outcome) -> Result<Outcome, Failure> {
    Ok(match outcome {
        Outcome::Show(view) => Outcome::Show(checked_view(view)),
        Outcome::Open(at) => match place(Some(at)) {
            Some(at) => Outcome::Open(at),
            // Its fault, not its answer: a failure, said in Clew's words.
            None => return Err(Failure::Stopped(OUTSIDE_PLACE.into())),
        },
        Outcome::Say(text) => Outcome::Say(line(&text)),
        Outcome::Nothing => Outcome::Nothing,
    })
}

/// A view, checked: [`MAX_BLOCKS`] blocks, [`MAX_ROWS`] rows and
/// [`MAX_VIEW_TEXT`] bytes of text at most, each text capped, places
/// outside the project dropped. A view cut short ends with a notice saying
/// so, for the user to know there was more.
pub fn checked_view(view: View) -> View {
    /// What is left of a page's rows and text.
    struct Budget {
        rows: usize,
        text: usize,
    }
    impl Budget {
        /// Whether a row of `size` bytes still fits; counted if so.
        fn row(&mut self, size: usize) -> bool {
            if self.rows == 0 || size > self.text {
                return false;
            }
            self.rows -= 1;
            self.text -= size;
            true
        }
        /// Whether a block of `size` bytes of text still fits; counted if so.
        fn text(&mut self, size: usize) -> bool {
            if size > self.text {
                return false;
            }
            self.text -= size;
            true
        }
    }
    let mut left = Budget {
        rows: MAX_ROWS,
        text: MAX_VIEW_TEXT,
    };
    let mut cut = view.blocks.len() > MAX_BLOCKS;
    let mut blocks = Vec::new();
    for block in view.blocks.into_iter().take(MAX_BLOCKS) {
        let checked = match block {
            Block::Heading(words) => Block::Heading(line(&words)),
            Block::Text(words) => Block::Text(capped(&words, TEXT_CAP)),
            Block::Notice(n) => Block::Notice(Notice {
                level: n.level,
                text: capped(&n.text, TEXT_CAP),
            }),
            Block::Items(items) => {
                let total = items.len();
                let kept: Vec<Item> = items
                    .into_iter()
                    .map(|i| Item {
                        title: line(&i.title),
                        detail: i.detail.as_deref().map(line),
                        at: place(i.at),
                        tag: i.tag.as_deref().map(line),
                        level: i.level,
                    })
                    .take_while(|i| {
                        left.row(
                            i.title.len()
                                + i.detail.as_ref().map_or(0, String::len)
                                + i.tag.as_ref().map_or(0, String::len),
                        )
                    })
                    .collect();
                cut |= kept.len() < total;
                Block::Items(kept)
            }
            Block::Tree(tree) => {
                let total = tree.len();
                let kept: Vec<TreeRow> = tree
                    .into_iter()
                    .map(|r| TreeRow {
                        depth: r.depth.min(32),
                        title: line(&r.title),
                        detail: r.detail.as_deref().map(line),
                        at: place(r.at),
                        tag: r.tag.as_deref().map(line),
                    })
                    .take_while(|r| {
                        left.row(
                            r.title.len()
                                + r.detail.as_ref().map_or(0, String::len)
                                + r.tag.as_ref().map_or(0, String::len),
                        )
                    })
                    .collect();
                cut |= kept.len() < total;
                Block::Tree(kept)
            }
            Block::Table(t) => {
                let columns: Vec<String> = t
                    .columns
                    .iter()
                    .take(MAX_COLUMNS)
                    .map(|c| line(c))
                    .collect();
                let width = columns.len();
                let total = t.rows.len();
                let kept: Vec<Row> = t
                    .rows
                    .into_iter()
                    .map(|r| Row {
                        cells: r.cells.iter().take(width.max(1)).map(|c| line(c)).collect(),
                        at: place(r.at),
                    })
                    .take_while(|r| left.row(r.cells.iter().map(String::len).sum()))
                    .collect();
                cut |= kept.len() < total;
                Block::Table(Table {
                    rows: kept,
                    columns,
                })
            }
            Block::Facts(facts) => {
                let total = facts.len();
                let kept: Vec<Fact> = facts
                    .into_iter()
                    .map(|f| Fact {
                        name: line(&f.name),
                        value: line(&f.value),
                    })
                    .take_while(|f| left.row(f.name.len() + f.value.len()))
                    .collect();
                cut |= kept.len() < total;
                Block::Facts(kept)
            }
            Block::Code(c) => Block::Code(Code {
                language: c.language.as_deref().map(|l| capped(l, 32)),
                text: capped(&c.text, TEXT_CAP),
                at: place(c.at),
            }),
        };
        // A block of text spends the same budget as rows do.
        let size = match &checked {
            Block::Heading(t) | Block::Text(t) => t.len(),
            Block::Notice(n) => n.text.len(),
            Block::Code(c) => c.text.len(),
            _ => 0,
        };
        if !left.text(size) {
            cut = true;
            break;
        }
        blocks.push(checked);
    }
    if cut {
        blocks.push(Block::Notice(Notice {
            level: Level::Info,
            text: CUT_NOTICE.into(),
        }));
    }
    View {
        title: line(&view.title),
        blocks,
    }
}

/// A graph, checked: [`MAX_NODES`] nodes and [`MAX_EDGES`] edges at most,
/// an edge only between nodes there are, places outside the project
/// dropped.
pub fn checked_graph(graph: Graph) -> Graph {
    let nodes: Vec<GraphNode> = graph
        .nodes
        .into_iter()
        .take(MAX_NODES)
        .map(|n| GraphNode {
            label: line(&n.label),
            kind: capped(&n.kind, 64),
            at: place(n.at),
            detail: capped(&n.detail, 4096),
        })
        .collect();
    let n = nodes.len() as u32;
    let edges = graph
        .edges
        .into_iter()
        .filter(|e| e.from < n && e.to < n)
        .take(MAX_EDGES)
        .map(|e| GraphEdge {
            from: e.from,
            to: e.to,
            label: line(&e.label),
        })
        .collect();
    Graph { nodes, edges }
}

/// Where a plugin's own store is; refused for what is not an id.
pub fn store_dir(data: &Path, id: &str) -> Result<PathBuf, String> {
    Ok(crate::store::plugins_dir(data)
        .join("data")
        .join(crate::store::checked_id(id)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that only counts the log lines it is given.
    #[derive(Default)]
    struct Counting(std::sync::Mutex<Vec<String>>);

    impl Host for Counting {
        fn info(&self) -> ProjectInfo {
            ProjectInfo {
                name: String::new(),
                files: 0,
                partial: false,
                remote: false,
            }
        }
        fn files(&self) -> Vec<String> {
            Vec::new()
        }
        fn read(&self, _: &str) -> Result<String, String> {
            Err(String::new())
        }
        fn outline(&self, _: &str) -> Result<Vec<Symbol>, String> {
            Ok(Vec::new())
        }
        fn search(&self, _: &Query) -> Result<Vec<Hit>, String> {
            Ok(Vec::new())
        }
        fn git_log(&self, _: Option<&str>, _: u32) -> Result<Vec<Commit>, String> {
            Ok(Vec::new())
        }
        fn git_blame(&self, _: &str) -> Result<Vec<BlameLine>, String> {
            Ok(Vec::new())
        }
        fn log(&self, _: Level, message: &str) {
            self.0.lock().unwrap().push(message.to_string());
        }
    }

    /// A plugin logging in a loop fills no more of the window's memory
    /// than a run's share: the lines past it are counted, not kept.
    #[test]
    fn a_run_passes_on_a_bounded_log() {
        use bindings::clew::plugin::host::Host as _;
        let counting = Arc::new(Counting::default());
        let mut state = State {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limiter: Limiter {
                memory: 0,
                used: 0,
                hit: false,
            },
            host: counting.clone(),
            store: None,
            clock: RunClock::until(Duration::from_secs(1)),
            read: 0,
            logged: 0,
            logged_bytes: 0,
            log_dropped: 0,
        };
        for i in 0..5000 {
            state.log(Level::Info, format!("line {i}"));
        }
        assert_eq!(counting.0.lock().unwrap().len(), LOG_LINES_PER_CALL);
        assert_eq!(state.log_dropped, 5000 - LOG_LINES_PER_CALL);
        // Long lines meet the byte budget first.
        let counting = Arc::new(Counting::default());
        state.host = counting.clone();
        (state.logged, state.logged_bytes, state.log_dropped) = (0, 0, 0);
        for _ in 0..200 {
            state.log(Level::Info, "x".repeat(10_000));
        }
        let kept = counting.0.lock().unwrap();
        assert_eq!(kept.len(), LOG_BYTES_PER_CALL / 4096);
        assert!(kept.iter().map(String::len).sum::<usize>() <= LOG_BYTES_PER_CALL);
    }

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(capped("hello", 10), "hello");
        let cut = capped("日本語のテキスト", 8);
        assert!(cut.len() <= 8 && cut.ends_with('…'), "{cut}");
        assert_eq!(line("a\nb\r\nc"), "a b  c");
    }

    #[test]
    fn the_backtrace_hint_is_not_logged() {
        let stderr = "thread '<unnamed>' (1) panicked at src/lib.rs:12:5:\nno such file\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(
            without_backtrace_hint(stderr),
            "thread '<unnamed>' (1) panicked at src/lib.rs:12:5:\nno such file"
        );
        assert_eq!(
            without_backtrace_hint("a note: of its own"),
            "a note: of its own"
        );
    }

    #[test]
    fn a_panic_is_read_from_stderr() {
        let stderr = "\nthread '<unnamed>' panicked at src/lib.rs:12:5:\nno such file\nnote: run with `RUST_BACKTRACE=1`\n";
        assert_eq!(
            panic_message(stderr).unwrap(),
            "no such file (at src/lib.rs:12:5)"
        );
        assert_eq!(panic_message("all fine"), None);
    }

    /// Output cut at the cap keeps its whole lines; one line longer than
    /// the cap keeps its start, without the character cut partway.
    #[test]
    fn output_cut_at_the_cap_keeps_whole_lines_or_a_start() {
        assert_eq!(kept_output("one\ntwo\nthr", true), "one\ntwo");
        assert_eq!(kept_output("one\ntwo\nthr", false), "one\ntwo\nthr");
        assert_eq!(
            kept_output("{\"a\": 1, \"b\u{FFFD}", true),
            "{\"a\": 1, \"b"
        );
    }

    #[test]
    fn places_outside_the_project_are_dropped() {
        let at = |p: &str| {
            Some(Location {
                path: p.into(),
                line: 3,
            })
        };
        assert!(place(at("src/main.rs")).is_some());
        for bad in ["../secret", "/etc/passwd", "", "a/../../b"] {
            assert!(place(at(bad)).is_none(), "{bad}");
        }
        let open = checked_outcome(Outcome::Open(Location {
            path: "../x".into(),
            line: 1,
        }));
        // Refused by Clew, in Clew's words (translated where shown): not
        // the plugin's own answer.
        assert!(matches!(open, Err(Failure::Stopped(why)) if why == OUTSIDE_PLACE));
    }

    #[test]
    fn a_view_is_capped() {
        let items = (0..MAX_ROWS + 50)
            .map(|i| Item {
                title: format!("row {i}\nsecond line"),
                detail: None,
                at: Some(Location {
                    path: "../out".into(),
                    line: 1,
                }),
                tag: None,
                level: None,
            })
            .collect();
        let more = vec![Item {
            title: "never shown".into(),
            detail: None,
            at: None,
            tag: None,
            level: None,
        }];
        let view = checked_view(View {
            title: "t".repeat(5000),
            blocks: vec![Block::Items(items), Block::Items(more)],
        });
        assert!(view.title.len() <= LINE_CAP);
        let Block::Items(first) = &view.blocks[0] else {
            panic!()
        };
        assert_eq!(first.len(), MAX_ROWS);
        assert_eq!(first[0].title, "row 0 second line");
        assert!(first[0].at.is_none());
        let Block::Items(second) = &view.blocks[1] else {
            panic!()
        };
        assert!(second.is_empty());
        // Cut short, the page says so.
        assert!(matches!(
            view.blocks.last(),
            Some(Block::Notice(n)) if n.text.contains("the rest is left out")
        ));

        // Fewer rows, but long ones: the page's text runs out first.
        let wide = (0..2000)
            .map(|_| Row {
                cells: vec!["x".repeat(LINE_CAP); MAX_COLUMNS],
                at: None,
            })
            .collect();
        let view = checked_view(View {
            title: "t".into(),
            blocks: vec![
                Block::Table(Table {
                    columns: vec!["c".into(); MAX_COLUMNS],
                    rows: wide,
                }),
                Block::Text("after".into()),
            ],
        });
        let Block::Table(table) = &view.blocks[0] else {
            panic!()
        };
        assert_eq!(table.rows.len(), MAX_VIEW_TEXT / (LINE_CAP * MAX_COLUMNS));
        let text: usize = table
            .rows
            .iter()
            .flat_map(|r| &r.cells)
            .map(String::len)
            .sum();
        assert!(text <= MAX_VIEW_TEXT);
        assert!(matches!(view.blocks.last(), Some(Block::Notice(_))));

        // A page within its limits is not marked.
        let small = checked_view(View {
            title: "t".into(),
            blocks: vec![Block::Text("hello".into())],
        });
        assert_eq!(small.blocks.len(), 1);
    }

    #[test]
    fn a_graph_keeps_only_edges_between_its_nodes() {
        let node = |l: &str| GraphNode {
            label: l.into(),
            kind: "k".into(),
            at: None,
            detail: String::new(),
        };
        let graph = checked_graph(Graph {
            nodes: vec![node("a"), node("b")],
            edges: vec![
                GraphEdge {
                    from: 0,
                    to: 1,
                    label: "ok".into(),
                },
                GraphEdge {
                    from: 1,
                    to: 2,
                    label: "dangling".into(),
                },
                GraphEdge {
                    from: 7,
                    to: 0,
                    label: "dangling".into(),
                },
            ],
        });
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].label, "ok");
    }

    #[test]
    fn store_keys_are_safe_file_names() {
        assert_eq!(key_file("a/../b").unwrap(), "612f2e2e2f62.bin");
        assert!(key_file("").is_none());
        assert!(key_file(&"k".repeat(KEY_CAP + 1)).is_none());
    }

    #[test]
    fn memory_is_limited_across_growths() {
        let mut limiter = Limiter {
            memory: 100,
            used: 0,
            hit: false,
        };
        assert!(limiter.memory_growing(0, 60, None).unwrap());
        assert!(limiter.memory_growing(60, 90, None).unwrap());
        assert!(!limiter.memory_growing(90, 120, None).unwrap());
        assert!(limiter.hit);
    }

    /// Tables are counted in the same budget as memories, and each memory
    /// and table grows it by its own growth: a table of a million elements
    /// cost nothing, and a second memory was counted as replacing the first.
    #[test]
    fn tables_and_every_memory_share_the_budget() {
        let mut limiter = Limiter {
            memory: 1000,
            used: 0,
            hit: false,
        };
        assert!(limiter.memory_growing(0, 400, None).unwrap());
        assert!(limiter.memory_growing(0, 400, None).unwrap());
        assert_eq!(limiter.used, 800);
        assert!(limiter.table_growing(0, 20, None).unwrap());
        assert_eq!(limiter.used, 800 + 20 * TABLE_ELEMENT_BYTES);
        assert!(!limiter.table_growing(20, 50, None).unwrap());
        assert!(limiter.hit);
        assert_eq!(limiter.used, 800 + 20 * TABLE_ELEMENT_BYTES);
    }
}
