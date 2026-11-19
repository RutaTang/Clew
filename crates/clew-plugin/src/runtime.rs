//! Running plugins, in Clew's own process, with wasmtime.
//!
//! - One [`Runtime`] per process: one engine, and a thread that ticks its
//!   epoch every 10 ms, which is how a call's deadline is kept.
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
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store, Trap};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
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
}

/// How long a call may run, and how much memory it may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub deadline: Duration,
    pub memory: usize,
}

impl Limits {
    /// For a command.
    pub const COMMAND: Limits = Limits {
        deadline: Duration::from_secs(30),
        memory: 256 * 1024 * 1024,
    };
    /// For a graph, which reads the whole project.
    pub const GRAPH: Limits = Limits {
        deadline: Duration::from_secs(60),
        memory: 512 * 1024 * 1024,
    };
}

/// Why a call gave nothing back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The plugin said it could not, in its own words.
    Plugin(String),
    /// It was stopped, or it crashed: why, in a sentence.
    Stopped(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Plugin(why) | Failure::Stopped(why) => f.write_str(why),
        }
    }
}

/// The plugin runtime: one per process.
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
        if !clew_rules::plugin::may_link(inspected.needs, granted) {
            let missing = inspected.needs & !granted;
            return Err(format!(
                "the plugin needs a permission it was not granted: {}",
                permission_names(missing).join(", ")
            ));
        }
        let component = self.compile(bytes, &inspected.sha256)?;
        let mut linker = Linker::<State>::new(&self.engine);
        link(&mut linker, granted).map_err(|e| format!("could not link the plugin: {e}"))?;
        let pre = linker
            .instantiate_pre(&component)
            .map_err(|e| format!("could not link the plugin: {e}"))?;
        let pre = bindings::PluginPre::new(pre)
            .map_err(|e| format!("the plugin does not export what Clew calls: {e}"))?;
        Ok(Loaded {
            pre,
            store: self
                .data
                .as_ref()
                .and_then(|d| store_dir(d, &inspected.manifest.plugin.id).ok()),
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
            read: 0,
        };
        let mut store = Store::new(self.pre.engine(), state);
        store.limiter(|s| &mut s.limiter);
        let ticks = (limits.deadline.as_millis() / TICK.as_millis()).max(1) as u64;
        store.set_epoch_deadline(ticks);
        store.epoch_deadline_trap();

        let result = self
            .pre
            .instantiate(&mut store)
            .and_then(|plugin| f(&plugin, &mut store));
        forward_output(host.as_ref(), Level::Info, &stdout);
        forward_output(host.as_ref(), Level::Warning, &stderr);
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(why)) => Err(Failure::Plugin(capped(&why, 4096))),
            Err(e) => Err(stopped(&e, limits, store.data().limiter.hit, &stderr)),
        }
    }
}

/// Why a call that trapped stopped, in a sentence.
fn stopped(e: &wasmtime::Error, limits: Limits, out_of_memory: bool, stderr: &Capture) -> Failure {
    if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
        return Failure::Stopped(format!(
            "the plugin took longer than {} s and was stopped",
            limits.deadline.as_secs()
        ));
    }
    if out_of_memory {
        return Failure::Stopped(format!(
            "the plugin needed more than {} MB of memory and was stopped",
            limits.memory / (1024 * 1024)
        ));
    }
    if let Some(panic) = panic_message(&stderr.text()) {
        return Failure::Stopped(format!("the plugin crashed: {panic}"));
    }
    let why = match e.downcast_ref::<Trap>() {
        Some(trap) => trap.to_string(),
        None => e.to_string(),
    };
    Failure::Stopped(format!("the plugin crashed: {}", capped(&why, 512)))
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

/// Write what a call printed to the plugin's log.
fn forward_output(host: &dyn Host, level: Level, capture: &Capture) {
    let text = capture.text();
    let text = text.trim();
    if !text.is_empty() {
        host.log(level, text);
    }
}

/// What one call's instance holds.
struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limiter: Limiter,
    host: Arc<dyn Host>,
    /// The plugin's own store's folder.
    store: Option<PathBuf>,
    /// The bytes of the project read so far.
    read: u64,
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

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let after = self.used - current.min(self.used) + desired;
        if after > self.memory {
            self.hit = true;
            return Ok(false);
        }
        self.used = after;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 1_000_000)
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
        self.host.log(level, &capped(&message, 4096));
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
/// after is accepted and dropped.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&bytes).into_owned()
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
}

impl tokio::io::AsyncWrite for Capture {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let room = OUTPUT_CAP.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..buf.len().min(room)]);
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
            None => {
                return Err(Failure::Plugin(
                    "the plugin asked to open a place outside the project".into(),
                ));
            }
        },
        Outcome::Say(text) => Outcome::Say(line(&text)),
        Outcome::Nothing => Outcome::Nothing,
    })
}

/// A view, checked: [`MAX_BLOCKS`] blocks and [`MAX_ROWS`] rows at most, each
/// line and text capped, places outside the project dropped.
pub fn checked_view(view: View) -> View {
    let mut rows = MAX_ROWS;
    let mut take = |n: usize| {
        let k = n.min(rows);
        rows -= k;
        k
    };
    let blocks = view
        .blocks
        .into_iter()
        .take(MAX_BLOCKS)
        .map(|block| match block {
            Block::Heading(text) => Block::Heading(line(&text)),
            Block::Text(text) => Block::Text(capped(&text, TEXT_CAP)),
            Block::Notice(n) => Block::Notice(Notice {
                level: n.level,
                text: capped(&n.text, TEXT_CAP),
            }),
            Block::Items(items) => {
                let k = take(items.len());
                Block::Items(
                    items
                        .into_iter()
                        .take(k)
                        .map(|i| Item {
                            title: line(&i.title),
                            detail: i.detail.as_deref().map(line),
                            at: place(i.at),
                            tag: i.tag.as_deref().map(line),
                            level: i.level,
                        })
                        .collect(),
                )
            }
            Block::Tree(tree) => {
                let k = take(tree.len());
                Block::Tree(
                    tree.into_iter()
                        .take(k)
                        .map(|r| TreeRow {
                            depth: r.depth.min(32),
                            title: line(&r.title),
                            detail: r.detail.as_deref().map(line),
                            at: place(r.at),
                            tag: r.tag.as_deref().map(line),
                        })
                        .collect(),
                )
            }
            Block::Table(t) => {
                let columns: Vec<String> = t
                    .columns
                    .iter()
                    .take(MAX_COLUMNS)
                    .map(|c| line(c))
                    .collect();
                let width = columns.len();
                let k = take(t.rows.len());
                Block::Table(Table {
                    rows: t
                        .rows
                        .into_iter()
                        .take(k)
                        .map(|r| Row {
                            cells: r.cells.iter().take(width.max(1)).map(|c| line(c)).collect(),
                            at: place(r.at),
                        })
                        .collect(),
                    columns,
                })
            }
            Block::Facts(facts) => {
                let k = take(facts.len());
                Block::Facts(
                    facts
                        .into_iter()
                        .take(k)
                        .map(|f| Fact {
                            name: line(&f.name),
                            value: line(&f.value),
                        })
                        .collect(),
                )
            }
            Block::Code(c) => Block::Code(Code {
                language: c.language.as_deref().map(|l| capped(l, 32)),
                text: capped(&c.text, TEXT_CAP),
                at: place(c.at),
            }),
        })
        .collect();
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

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(capped("hello", 10), "hello");
        let cut = capped("日本語のテキスト", 8);
        assert!(cut.len() <= 8 && cut.ends_with('…'), "{cut}");
        assert_eq!(line("a\nb\r\nc"), "a b  c");
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
        assert!(matches!(open, Err(Failure::Plugin(_))));
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
}
