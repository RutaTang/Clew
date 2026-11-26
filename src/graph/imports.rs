//! The import graph: a file→file dependency map derived from each file's
//! import statements.
//!
//! Where the call graph is symbol→symbol and needs a language server, the import
//! graph is purely structural and computed locally from tree-sitter — so the
//! whole project's dependency map is available the instant a scan finishes, and
//! offline. It fits clew's incremental core: what a file writes down as
//! imports is a pure function of its own bytes, so a change re-extracts only
//! that file. Where those imports RESOLVE also depends on the file set, the
//! project's metadata files and — for Rust — the other files a `use` is
//! followed through, which is what [`ImportGraph::apply`] tracks to re-resolve
//! only what a change can have moved. Reverse edges (who imports it) are just
//! the inversion.
//!
//! Two stages, deliberately separated:
//!   * **Extraction** ([`clew_core::imports::imports_in`]) — a tree-sitter pass that reads the raw
//!     import specifiers a file writes down. Pure per file, cacheable.
//!   * **Resolution** ([`Resolver`]) — per-language rules that map a specifier
//!     to a project file ([`Target::Internal`]), an external package
//!     ([`Target::External`], not penetrated), or nothing ([`Target::Unresolved`]).
//!     This runs over the known file set, so creating/deleting a file can change
//!     how *other* files resolve; the graph re-resolves on such structural change.

use clew_core::imports::strip_jsonc;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use clew_core::imports::{ERLANG_LIB, PY_NAME_SEP};
use clew_core::rustscope::RustUse;

use crate::{tr, trn};

// Extraction lives in clew-core (shared with the server's project-symbol
// snapshot, so a remote client receives raw imports over the protocol
// instead of reading remote-pathed files off its own disk). The index takes
// imports from its single parse per file (`index::analyze_file`); the
// stand-alone extractor stays reachable here for the tests that pin it.
pub use clew_core::imports::RawImport;
#[cfg(test)]
pub use clew_core::imports::imports_of;

/// Where an import points once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A file (or, for Go, a package directory) inside the project.
    Internal(PathBuf),
    /// An external package / standard library — shown as a leaf, never penetrated.
    External(String),
    /// A specifier we couldn't map to anything (e.g. a `mod x;` whose file is
    /// missing). Kept rather than dropped, so the graph is honest.
    Unresolved(String),
}

/// What an edge means. Kept apart because they answer different questions:
/// a Rust `mod x;` says "x is part of me" (ownership — the module TREE), an
/// import says "I depend on it". Every child module that uses its parent's
/// items (`use super::…`, `use crate::…`) closes a loop with its `mod` edge,
/// so cycle detection over every kind flagged nearly every Rust module.
/// Cycle detection counts [`EdgeKind::Use`] only
/// ([`EdgeKind::is_dependency`]); the graph and the Imports tree keep all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// An import / `use` / `require` / JS re-export: a dependency.
    Use,
    /// A Rust `mod x;` declaration: module ownership, not a dependency.
    ModDecl,
    /// A Rust re-export (`pub use a::B;`, `pub(crate) use m::*;`): the module
    /// publishing what lives elsewhere — a facade over the module tree, not
    /// something its own code depends on. A crate root that re-exports its
    /// modules and children that `use crate::…` its items would otherwise
    /// all sit in one "cycle".
    Reexport,
    /// A Rust `use` of what an enclosing module holds — its namespace
    /// (`use super::*;`, `use crate::*;`) or an item it defines
    /// (`use super::Config;`, `use crate::Error;` of one in the crate root).
    /// A child module is part of the module that declares it: it reaching
    /// into its own enclosing module is the module tree's inside, not one
    /// file depending on another, and Rust allows it without a word. With
    /// the ancestor's use of the child, it closed a "cycle" for every module
    /// whose children share its types — the normal shape of a Rust crate
    /// (salvo's `compression` crate: `lib.rs` uses its `encoder::Encoder`,
    /// `encoder.rs` the root's `CompressionAlgo`).
    Enclosing,
    /// A Rust `use` of a name found nowhere — no module file, no re-export,
    /// no item the symbol index lists — taken to be an item of the module it
    /// was reached through, by elimination: a name an external glob supplies,
    /// a macro-generated item, a typo. Shown where it points, but not a
    /// dependency: an edge nothing confirms must not close a cycle (it used
    /// to, through the crate root, for every such name).
    Inferred,
}

impl EdgeKind {
    /// Whether an edge of this kind is a dependency — what import cycles
    /// are made of.
    pub fn is_dependency(self) -> bool {
        self == EdgeKind::Use
    }
}

/// A resolved out-edge: which file/package an import points to, and where the
/// statement sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub target: Target,
    pub specifier: String,
    pub line: usize,
    pub kind: EdgeKind,
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Everything resolution needs about the project's file layout: the set of files
/// (for existence checks), the Rust crate source root (where `crate::` starts),
/// and the Go module prefix (from `go.mod`). Built off the UI thread by the
/// import-graph job whenever one of these inputs changed, and reused by every
/// job after it until the next change: cheap in-memory lookups after that.
/// What other FILES say (Rust `use`s and items) is the graph's [`RustCtx`].
#[derive(Debug)]
pub struct Resolver {
    root: PathBuf,
    files: HashSet<PathBuf>,
    dirs: HashSet<PathBuf>,
    /// Every Rust crate-root FILE in the project (a workspace has one per
    /// member, a package one per target). `crate::` resolves against the
    /// importing file's own crate root — not a single project-wide one.
    /// Files, not directories: every crate root's modules live beside that
    /// root, whether it is `src/lib.rs`, `src/bin/x.rs`, or `examples/demo.rs`.
    /// A plain module file's children instead live in its stem's directory.
    rust_crate_roots: Vec<PathBuf>,
    /// The `module` line from `go.mod`, if any.
    go_module: Option<String>,
    /// The package `name:` from `pubspec.yaml`, so a Dart file's self-referential
    /// `package:<name>/…` import resolves back into `lib/`.
    dart_package: Option<String>,
    /// `tsconfig.json` / `jsconfig.json` module-resolution settings, nearest
    /// (deepest) first. A local project's are read off disk
    /// ([`Resolver::new`]); a remote project's are fetched from its host and
    /// supplied with [`Resolver::with_ts_configs`] once they arrive — shared
    /// with the window that holds them, not copied per job. Empty until
    /// then, and for projects without one.
    ts_configs: Arc<TsConfigs>,
    /// Each package directory with its files: a Go package's, its tests
    /// aside — what an import of the package, a directory, reaches — and a
    /// Java package's, which a `*` import reaches. From the project's files,
    /// every one of them; the graph only holds those with imports.
    packages: Arc<HashMap<PathBuf, Vec<PathBuf>>>,
    /// Each Lean module name a project file can be imported as, with the
    /// files that can be it: `A/B/C.lean` is `C`, `B.C` and `A.B.C`, as the
    /// directory it is imported from (its package's source root) is not
    /// written down anywhere clew reads. See [`Resolver::resolve_lean`].
    lean_modules: Arc<HashMap<String, Vec<PathBuf>>>,
    /// The folders a Lean module may be found under, as the project's
    /// lakefiles say: each Lake package's own and the `srcDir`s it names.
    /// `None` for a project with no lakefile, where any folder may be.
    lean_roots: Option<Vec<PathBuf>>,
    /// The project's conventional Java source roots, sorted: every
    /// `src/main/java` and `src/test/java`, one pair per module of a
    /// multi-module build. See [`Resolver::resolve_java`].
    java_roots: Vec<PathBuf>,
    /// The PSR-4 maps of the project's `composer.json` files: each namespace
    /// prefix (ending in `\`; empty for a fallback) with the directories it
    /// maps to, the longest prefix first. Read off disk for a local project
    /// only — a remote one resolves without them. See
    /// [`Resolver::resolve_php`].
    php_psr4: Vec<(String, Vec<PathBuf>)>,
    /// The Haskell source roots that exist: what the project's `.cabal` and
    /// `package.yaml` files name (a local project's only, as above), then the
    /// conventional ones. See [`Resolver::resolve_haskell`].
    haskell_roots: Vec<PathBuf>,
    /// Each library a Cargo package of the project's has, by the name code
    /// calls it (`use name::…`), with its root file: a workspace's crates
    /// use one another's items through these. What the project's manifests
    /// say ([`Resolver::with_manifests`]); empty without them.
    rust_libs: HashMap<String, PathBuf>,
    /// The modules each `build.zig` names, by the folder it is in (the
    /// deepest first): a file `@import`s one its nearest build.zig names by
    /// that name (`@import("greet")`), the module's root file — a nested
    /// package sees its own build's names, not those of the build around it. What the project's
    /// manifests say ([`Resolver::with_manifests`]); empty without them.
    zig_modules: Vec<(PathBuf, HashMap<String, PathBuf>)>,
    /// Each Dart package of the project by its name, with its `lib/`
    /// folder: what `package:<name>/…` names. See [`Resolver::resolve_dart`].
    dart_packages: HashMap<String, PathBuf>,
    /// The Python source roots the project's `pyproject.toml`s name
    /// (`src`, `lib`, maturin's `python`, pytest's `pythonpath`, …), that
    /// exist: where an absolute import is looked for after the project's
    /// root. See [`Resolver::resolve_python`].
    python_roots: Vec<PathBuf>,
    /// Each Erlang module a project file defines (`m.erl` is `m`), with the
    /// files that do, sorted.
    erlang_modules: HashMap<String, Vec<PathBuf>>,
    /// Each Erlang header's file name (`x.hrl`) with the files so named.
    erlang_headers: HashMap<String, Vec<PathBuf>>,
    /// Each file a C or C++ `#include` can name (a header, or a source file
    /// included whole), by its file name, with the files so named, sorted.
    c_headers: HashMap<String, Vec<PathBuf>>,
    /// Each Erlang application the project holds by its `src/<app>.app.src`,
    /// with the application's directory — whatever that directory is called.
    erlang_apps: HashMap<String, PathBuf>,
    /// Each JavaScript package of the project's, by its name, with its
    /// folder and what it exports ([`clew_protocol::JsPackage`]): a
    /// workspace's packages import one another by name, not by path.
    js_packages: HashMap<String, JsPackageAt>,
}

/// One of the project's JavaScript packages: its folder, and each subpath
/// it exports with the files that may be it.
type JsPackageAt = (PathBuf, Vec<(String, Vec<String>)>);

impl Resolver {
    /// Whether `path` is a file one of this resolver's tsconfig/jsconfig
    /// settings was built from ([`TsConfigs::reads`]): an edit to it changes
    /// how aliases resolve, so the graph must re-resolve against a resolver
    /// that reads it again — also for a base no naming rule recognises.
    pub fn reads_config(&self, path: &Path) -> bool {
        self.ts_configs.reads(path)
    }

    /// What the tsconfig/jsconfig caps left out of this resolver's configs
    /// ([`TsConfigs::cap_note`]).
    pub fn ts_cap_note(&self) -> Option<String> {
        self.ts_configs.cap_note()
    }
}

/// `compilerOptions.paths`: each pattern with its targets, in file order.
pub type TsPaths = Vec<(String, Vec<String>)>;

/// The module-resolution part of one `tsconfig.json` / `jsconfig.json`
/// (after following `extends`): what `baseUrl` and `paths` map specifiers to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TsConfig {
    /// Directory holding the config; it applies to files beneath it.
    pub dir: PathBuf,
    /// `compilerOptions.baseUrl`, absolute.
    pub base_url: Option<PathBuf>,
    /// `compilerOptions.paths` patterns in file order, each with its targets.
    pub paths: TsPaths,
    /// What `paths` targets are relative to, as TypeScript decides it: the
    /// `baseUrl` in effect after following `extends` (wherever in the chain
    /// it was set), else the directory of the config that declared `paths`.
    pub paths_base: PathBuf,
}

/// The tsconfig/jsconfig settings one resolve reads, deepest directory first
/// (so the first whose directory holds a file is its nearest), with the files
/// they were built from. Built once per resolve ([`ts_configs_from`]) and
/// shared (`Arc`) from then on: by the resolver, and for a remote project by
/// the window and every job it starts. Derefs to the configs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TsConfigs {
    configs: Vec<TsConfig>,
    /// Every file the configs were built from: each config, then each base
    /// an `extends` chain names, read or not — a change to any of them (a
    /// `base.json`, which no naming rule would catch) changes the configs.
    /// One set for all of them, however many configs extend the same chain,
    /// and at most [`MAX_TS_SOURCES`] files.
    sources: HashSet<PathBuf>,
    /// What the caps left out of them.
    left_out: TsLeftOut,
}

/// What the caps left out of one resolve's configs: nothing, in any real
/// project — said when it is not ([`TsConfigs::cap_note`]), since an alias
/// such a file would have mapped then resolves as a package.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TsLeftOut {
    /// Configs past the first [`MAX_TS_CONFIGS`]: not read.
    configs: usize,
    /// `extends` entries past the first [`MAX_TS_EXTENDS_ENTRIES`] of an
    /// array: ignored.
    extends: usize,
    /// Bases named once [`MAX_TS_SOURCES`] files were admitted: not read.
    bases: usize,
    /// The configs and bases that are there and were not read, by their
    /// project-relative paths, with why — read off this disk
    /// ([`read_ts_configs`]), or not sent by a remote host
    /// ([`load_remote_ts_configs`]): named, as only the user can do anything
    /// about them. Each was taken for one that is not there, without a word.
    files: Vec<(String, TsNotRead)>,
}

/// Why a config file that is there was not read ([`TsLeftOut::files`]).
/// Read again, it reads the same until the file changes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TsNotRead {
    /// Too large to read, with its size in bytes.
    TooLarge(u64),
    /// No plain text file of the project, as what it is: not UTF-8, a link,
    /// a FIFO or a device, or reached through a link out of it.
    Refused(clew_protocol::Refusal),
    /// It could not be read — this user may not, or the read failed — with
    /// the error.
    Unreadable(String),
}

impl TsNotRead {
    /// Why a local read of a config came back empty ([`read_ts_configs`]).
    fn of_read(e: &clew_core::statefile::ReadError) -> TsNotRead {
        use clew_core::explain::Unexplainable;
        match Unexplainable::of_read(e) {
            Some(Unexplainable::TooLarge(size)) => TsNotRead::TooLarge(size),
            Some(Unexplainable::Refused(why)) => TsNotRead::Refused(why),
            None => TsNotRead::Unreadable(e.to_string()),
        }
    }
}

/// Most files a note on the configs names ([`TsConfigs::cap_note`]); the
/// rest are counted.
const MAX_NAMED_TS_FILES: usize = 3;

impl TsConfigs {
    /// Whether `path` is one of the files the configs were built from: one
    /// lookup, asked for every changed path on the window's thread.
    pub fn reads(&self, path: &Path) -> bool {
        self.sources.contains(path)
    }

    /// A one-line note on what the caps left out, or `None` when nothing
    /// was: "tsconfig/jsconfig: 3 more configs not read (limit 64)", or
    /// "tsconfig/jsconfig: tsconfig.json (too large, 600 KiB) not read".
    pub fn cap_note(&self) -> Option<String> {
        let TsLeftOut {
            configs,
            extends,
            bases,
            ref files,
        } = self.left_out;
        let mut parts = Vec::new();
        if configs > 0 {
            parts.push(trn!(
                "1 more config not read (limit {limit})",
                "{n} more configs not read (limit {limit})",
                configs,
                limit = MAX_TS_CONFIGS
            ));
        }
        if extends > 0 {
            parts.push(trn!(
                "1 `extends` entry past the first {limit} of an array ignored",
                "{n} `extends` entries past the first {limit} of an array ignored",
                extends,
                limit = MAX_TS_EXTENDS_ENTRIES
            ));
        }
        if bases > 0 {
            parts.push(trn!(
                "1 more base config not read (limit {limit} files)",
                "{n} more base configs not read (limit {limit} files)",
                bases,
                limit = MAX_TS_SOURCES
            ));
        }
        if !files.is_empty() {
            let named: Vec<String> = files
                .iter()
                .take(MAX_NAMED_TS_FILES)
                .map(|(rel, why)| match why {
                    TsNotRead::TooLarge(size) => {
                        tr!(
                            "{file} (too large, {size} KiB)",
                            file = rel,
                            size = size.div_ceil(1024)
                        )
                    }
                    // Each as what it is: a Latin-1 config and a link out of
                    // the project were both "not a plain text file".
                    TsNotRead::Refused(clew_protocol::Refusal::NotUtf8) => {
                        tr!("{file} (not UTF-8)", file = rel)
                    }
                    TsNotRead::Refused(clew_protocol::Refusal::NotPlainFile) => {
                        tr!("{file} (not a plain file)", file = rel)
                    }
                    TsNotRead::Refused(clew_protocol::Refusal::OutsideProject) => {
                        tr!("{file} (outside the project)", file = rel)
                    }
                    TsNotRead::Unreadable(why) => tr!(
                        "{file} (could not be read: {error})",
                        file = rel,
                        error = why
                    ),
                })
                .collect();
            parts.push(match files.len().saturating_sub(MAX_NAMED_TS_FILES) {
                0 => tr!("{files} not read", files = named.join(", ")),
                more => tr!(
                    "{files} and {count} more not read",
                    files = named.join(", "),
                    count = more
                ),
            });
        }
        (!parts.is_empty()).then(|| format!("tsconfig/jsconfig: {}", parts.join("; ")))
    }
}

impl std::ops::Deref for TsConfigs {
    type Target = [TsConfig];

    fn deref(&self) -> &[TsConfig] {
        &self.configs
    }
}

/// Configs assembled by hand, sorted the way a resolve sorts them. They
/// record no sources.
#[cfg(test)]
impl From<Vec<TsConfig>> for TsConfigs {
    fn from(mut configs: Vec<TsConfig>) -> Self {
        configs.sort_by_key(|c| std::cmp::Reverse(c.dir.components().count()));
        TsConfigs {
            configs,
            ..TsConfigs::default()
        }
    }
}

/// Rust facts about the whole file set that resolving one `use` can need:
/// each file's declared child modules (`mod x;`), its `use` declarations, and
/// the importable items it defines (from the symbol index — what a glob
/// re-export brings in).
///
/// Why: `use crate::codeview::CodeView` names a module the crate ROOT
/// re-exports (`pub use editor::codeview;`), `use crate::PreparedSeg` names an
/// item the root brings in by glob (`pub(crate) use app::model::*;`), and 2018
/// paths like `use editor::codeview;` start at a LOCAL module, not an external
/// crate — none of them can be resolved from the importing file alone.
///
/// Kept per file, in step with the graph's raw imports ([`ImportGraph::apply`]):
/// a batch replaces only the files it carries, and whether any of THEIR facts
/// changed is exactly whether other files may now resolve differently. Each
/// file's facts are shared, so copying the context costs a key per file.
#[derive(Debug, Default, Clone)]
pub struct RustCtx {
    files: HashMap<PathBuf, Arc<RustFacts>>,
}

/// One Rust file's facts in a [`RustCtx`].
#[derive(Debug, Default, PartialEq)]
struct RustFacts {
    uses: Vec<UseDecl>,
    mods: HashSet<String>,
    items: RustItems,
}

/// One `use` declaration of a file, decoded (see [`RustUse`]).
#[derive(Debug, PartialEq)]
struct UseDecl {
    /// The path; for a glob, the module it reads from.
    path: String,
    glob: bool,
    reexport: bool,
}

impl RustFacts {
    fn of(imports: &[RawImport], items: RustItems) -> Self {
        let mut facts = RustFacts {
            items,
            ..RustFacts::default()
        };
        for r in imports {
            if r.is_mod_decl {
                // Only the file's own children; `a::b` is inside inline `a`.
                let (name, _) = clew_core::rustscope::mod_decl_parts(&r.module);
                if !name.contains("::") {
                    facts.mods.insert(rust_identifier(name).to_string());
                }
            } else {
                let u = RustUse::parse(&r.module);
                facts.uses.push(UseDecl {
                    path: rust_path_segments(u.path).join("::"),
                    // An older extraction dropped a glob's star; the paths
                    // only a glob can import still say what it was.
                    glob: u.glob || u.names_an_enclosing_module(),
                    reexport: u.reexport,
                });
            }
        }
        facts
    }
}

impl RustCtx {
    /// Collect the Rust facts from every Rust file's raw imports (no items).
    pub fn new(
        raw: &HashMap<PathBuf, Vec<RawImport>>,
        lang_of: &impl Fn(&Path) -> Option<&'static str>,
    ) -> Self {
        let mut ctx = RustCtx::default();
        for (file, imports) in raw {
            if lang_of(file) == Some("rust") {
                ctx.set(file, RustFacts::of(imports, RustItems::new()));
            }
        }
        ctx
    }

    /// Record `file`'s facts. Returns whether they differ from what was held —
    /// including a file the context did not know — which is whether another
    /// file's `use` may now resolve differently.
    fn set(&mut self, file: &Path, facts: RustFacts) -> bool {
        if self.files.get(file).is_some_and(|held| **held == facts) {
            return false;
        }
        self.files.insert(file.to_path_buf(), Arc::new(facts));
        true
    }

    /// Forget `file`; whether it had facts to forget.
    fn remove(&mut self, file: &Path) -> bool {
        self.files.remove(file).is_some()
    }

    /// Whether `file` declares a child module `name` — `None` when the file's
    /// imports are unknown to this context.
    fn declares(&self, file: &Path, name: &str) -> Option<bool> {
        self.files.get(file).map(|f| f.mods.contains(name))
    }

    fn uses_of(&self, file: &Path) -> &[UseDecl] {
        self.files
            .get(file)
            .map(|f| f.uses.as_slice())
            .unwrap_or(&[])
    }

    /// Whether the symbol index lists `name` as an item `file` defines.
    fn defines(&self, file: &Path, name: &str) -> bool {
        self.files
            .get(file)
            .is_some_and(|f| f.items.contains(&name_key(name)))
    }
}

/// A [`RustCtx`] as ONE file's resolution reads it: every file whose facts it
/// looks up — found or not — is noted.
///
/// A Rust `use` resolves from the resolver (the file set, whose change
/// re-resolves everything anyway) and from the facts of the files it looks
/// up, nothing else. So a file needs resolving again exactly when the facts
/// of a file it read changed — or appeared: a lookup that found nothing is
/// noted too, since a file that gains the name would answer it
/// ([`ImportGraph::apply`]).
struct Consulted<'a> {
    ctx: &'a RustCtx,
    read: std::cell::RefCell<HashSet<PathBuf>>,
}

impl<'a> Consulted<'a> {
    fn new(ctx: &'a RustCtx) -> Self {
        Consulted {
            ctx,
            read: Default::default(),
        }
    }

    fn note(&self, file: &Path) {
        let mut read = self.read.borrow_mut();
        if !read.contains(file) {
            read.insert(file.to_path_buf());
        }
    }

    fn declares(&self, file: &Path, name: &str) -> Option<bool> {
        self.note(file);
        self.ctx.declares(file, name)
    }

    fn uses_of(&self, file: &Path) -> &'a [UseDecl] {
        self.note(file);
        self.ctx.uses_of(file)
    }

    fn defines(&self, file: &Path, name: &str) -> bool {
        self.note(file);
        self.ctx.defines(file, name)
    }

    /// The files read, sorted.
    fn into_read(self) -> Vec<PathBuf> {
        let mut read: Vec<PathBuf> = self.read.into_inner().into_iter().collect();
        read.sort();
        read
    }
}

/// Name keys of the importable items one Rust file defines (see
/// [`rust_item_keys`]). Kept as 64-bit keys: every file's set is compared on
/// each batch, and a large crate's item names are work a key spares.
pub type RustItems = HashSet<u64>;

/// The importable Rust items (types, traits, functions, modules, macros) that
/// `symbols` — one file's symbol-index entries — define, as the keys
/// resolution compares. Empty for a file that is not Rust.
pub fn rust_item_keys(path: &Path, symbols: &[crate::index::SymbolEntry]) -> RustItems {
    if path.extension().is_none_or(|e| e != "rs") {
        return RustItems::new();
    }
    symbols
        .iter()
        .filter(|s| is_importable_rust_item(&s.kind))
        .map(|s| name_key(&s.name))
        .collect()
}

/// A process-stable 64-bit key for a name (std's SipHash with fixed keys).
fn name_key(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rust_identifier(name).hash(&mut hasher);
    hasher.finish()
}

/// Raw identifiers escape a keyword in source; the escape is not part of the
/// module's filename or of the identifier Rust resolves.
fn rust_identifier(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

fn rust_path_segments(path: &str) -> Vec<&str> {
    path.split("::")
        .filter(|s| !s.is_empty())
        .map(rust_identifier)
        .collect()
}

/// Symbol kinds a Rust `use` can import by name. Methods live inside `impl`
/// blocks and cannot be; everything else the outline reports can.
fn is_importable_rust_item(kind: &str) -> bool {
    matches!(
        kind,
        "struct"
            | "enum"
            | "union"
            | "type"
            | "function"
            | "trait"
            | "module"
            | "macro"
            | "constant"
            | "static"
    )
}

/// How deep re-export chains are followed (`pub use a::b` where `a` itself
/// re-exports `b` …) before giving up; also the cycle guard.
const MAX_REEXPORT_HOPS: usize = 4;

/// What files declare themselves part of, for the imports that resolve
/// against it rather than the file set alone (declarations among the raw
/// imports, [`RawImport::is_mod_decl`]): the importer's own Java package,
/// which says where its source root is, and the C# namespaces the project's
/// files declare, which `using`s name.
#[derive(Debug, Clone, Copy, Default)]
pub struct Declared<'a> {
    package: Option<&'a str>,
    namespaces: Option<&'a Namespaces>,
}

/// Whether `raw`, an import of a `lang` file, is a declaration of what the
/// file is part of (a Java package, a C# namespace) rather than an import:
/// no edge of its own. A Rust `mod x;` is an edge — the module tree's.
fn is_declaration(lang: &str, raw: &RawImport) -> bool {
    raw.is_mod_decl && matches!(lang, "java" | "kotlin" | "scala" | "csharp" | "elixir")
}

/// The C# namespaces the graph's files declare (`namespace A.B { … }`,
/// `namespace A.B;`, recorded among their raw imports as declarations): what
/// a `using A.B;` reaches. A namespace is neither a file nor a directory —
/// any file anywhere may declare it, and several usually do — so a `using`
/// points at a node of its own ([`Resolver::namespace_node`]) that reaches
/// the namespace's files, as a Go import's directory reaches its package's.
/// An edge to each of the files instead would cost one per file per `using`,
/// and close a "cycle" between any two files whose namespaces use each
/// other. Shared per entry: copying it costs a key per namespace and per
/// declaring file.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Namespaces {
    /// Each namespace's node with the files declaring it, sorted.
    files: HashMap<PathBuf, Arc<Vec<PathBuf>>>,
    /// Each declaring file with the namespace nodes it declares, sorted.
    declared: HashMap<PathBuf, Arc<Vec<PathBuf>>>,
}

impl Namespaces {
    /// The files declaring the namespace `node`, if any do.
    fn files(&self, node: &Path) -> Option<&[PathBuf]> {
        self.files.get(node).map(|files| files.as_slice())
    }

    /// The namespace nodes `file` declares.
    fn of(&self, file: &Path) -> &[PathBuf] {
        self.declared
            .get(file)
            .map_or(&[], |nodes| nodes.as_slice())
    }

    /// Record that `file` declares the namespaces `nodes` (none: it declares
    /// none, or it is gone), and return the namespaces whose files that
    /// changed.
    fn set(&mut self, file: &Path, mut nodes: Vec<PathBuf>) -> Vec<PathBuf> {
        nodes.sort();
        nodes.dedup();
        let held = self.of(file).to_vec();
        if held == nodes {
            return Vec::new();
        }
        let left: Vec<PathBuf> = held
            .iter()
            .filter(|n| !nodes.contains(n))
            .cloned()
            .collect();
        let joined: Vec<PathBuf> = nodes
            .iter()
            .filter(|n| !held.contains(n))
            .cloned()
            .collect();
        for node in &left {
            let emptied = match self.files.get_mut(node) {
                Some(files) => {
                    Arc::make_mut(files).retain(|f| f != file);
                    files.is_empty()
                }
                None => false,
            };
            if emptied {
                self.files.remove(node);
            }
        }
        for node in &joined {
            let files = Arc::make_mut(self.files.entry(node.clone()).or_default());
            if let Err(at) = files.binary_search_by(|f| f.as_path().cmp(file)) {
                files.insert(at, file.to_path_buf());
            }
        }
        if nodes.is_empty() {
            self.declared.remove(file);
        } else {
            self.declared.insert(file.to_path_buf(), Arc::new(nodes));
        }
        left.into_iter().chain(joined).collect()
    }
}

impl Resolver {
    pub fn new(root: &Path, files: &[PathBuf]) -> Self {
        // A local project reads its module metadata straight off disk.
        let mut r = Self::with_meta(root, files, read_go_module(root), read_dart_package(root));
        r.ts_configs = Arc::new(read_ts_configs(root, files));
        let rels: Vec<String> = files
            .iter()
            .filter_map(|f| f.strip_prefix(root).ok())
            .map(clew_core::rel_string)
            .collect();
        r = r.with_manifests(&clew_core::imports::source_manifests(root, &rels));
        r
    }

    /// Replace the TypeScript/JavaScript path-mapping configs (for callers that
    /// obtained them another way, and for tests): shared, not copied.
    pub fn with_ts_configs(mut self, configs: impl Into<Arc<TsConfigs>>) -> Self {
        self.ts_configs = configs.into();
        self
    }

    /// Take what the project's package manifests say of where its sources
    /// are ([`clew_core::imports::source_manifests`]): `composer.json`'s
    /// PSR-4 maps, each directory made absolute against its file's, the
    /// longest prefix first (a prefix written without its closing `\` gets
    /// it, so `App` does not map `Application\…`); the Haskell source
    /// roots its package descriptions name; each Cargo package's library by
    /// its name, and the target roots Cargo's layout would not find; and the
    /// Python source roots `pyproject.toml`s name. A local project's are
    /// read off disk ([`Resolver::new`]); a remote one's arrive with the
    /// server's snapshot. A path a manifest names outside the project is
    /// not taken.
    pub fn with_manifests(mut self, manifests: &clew_protocol::SourceManifests) -> Self {
        let mut maps: Vec<(String, Vec<PathBuf>)> = Vec::new();
        for (rel, map) in &manifests.psr4 {
            let Some(base) = self.root.join(rel).parent().map(Path::to_path_buf) else {
                continue;
            };
            for (prefix, dirs) in map {
                let mut prefix = prefix.clone();
                if !prefix.is_empty() && !prefix.ends_with('\\') {
                    prefix.push('\\');
                }
                let dirs = dirs.iter().map(|d| normalize(&base.join(d))).collect();
                maps.push((prefix, dirs));
            }
        }
        maps.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        self.php_psr4 = maps;
        let declared: Vec<(PathBuf, Vec<String>)> = manifests
            .haskell
            .iter()
            .filter_map(|(rel, dirs)| {
                Some((self.root.join(rel).parent()?.to_path_buf(), dirs.clone()))
            })
            .collect();
        self.haskell_roots = haskell_roots(&self.root, &declared, &self.dirs);
        let inside = |base: &Path, path: &str| {
            let full = normalize(&base.join(path));
            full.starts_with(&self.root).then_some(full)
        };
        let mut libs = HashMap::new();
        let mut roots = std::mem::take(&mut self.rust_crate_roots);
        for (rel, package) in &manifests.cargo {
            let Some(base) = self.root.join(rel).parent().map(Path::to_path_buf) else {
                continue;
            };
            let lib = inside(&base, &package.lib_path).filter(|lib| self.has_file(lib));
            if let Some(lib) = lib {
                roots.push(lib.clone());
                // Two packages' libraries of one name: neither is meant for
                // certain, so the shallowest (read first) stands.
                libs.entry(package.lib_name.clone()).or_insert(lib);
            }
            roots.extend(
                package
                    .target_paths
                    .iter()
                    .filter_map(|path| inside(&base, path))
                    .filter(|file| self.has_file(file)),
            );
        }
        roots.sort();
        roots.dedup();
        self.rust_crate_roots = roots;
        self.rust_libs = libs;
        let mut zig: Vec<(PathBuf, HashMap<String, PathBuf>)> = Vec::new();
        for (rel, modules) in &manifests.zig {
            let Some(base) = self.root.join(rel).parent().map(Path::to_path_buf) else {
                continue;
            };
            let named: HashMap<String, PathBuf> = modules
                .iter()
                .filter_map(|(name, path)| {
                    let file = inside(&base, path).filter(|f| self.has_file(f))?;
                    Some((name.clone(), file))
                })
                .collect();
            zig.push((base, named));
        }
        zig.sort_by_key(|(dir, _)| std::cmp::Reverse(dir.components().count()));
        self.zig_modules = zig;
        let mut dart = HashMap::new();
        for (rel, name) in &manifests.dart {
            if let Some(lib) = self
                .root
                .join(rel)
                .parent()
                .map(|dir| dir.join("lib"))
                .filter(|lib| lib.starts_with(&self.root))
            {
                // Two packages of one name: the shallowest, read first.
                dart.entry(name.clone()).or_insert(lib);
            }
        }
        self.dart_packages = dart;
        let mut python_roots: Vec<PathBuf> = Vec::new();
        for (rel, named) in &manifests.python {
            let Some(base) = self.root.join(rel).parent().map(Path::to_path_buf) else {
                continue;
            };
            for dir in named.iter().filter_map(|d| inside(&base, d)) {
                if self.dirs.contains(&dir) && !python_roots.contains(&dir) {
                    python_roots.push(dir);
                }
            }
        }
        self.python_roots = python_roots;
        let mut lean_roots: Vec<PathBuf> = Vec::new();
        for (rel, dirs) in &manifests.lean {
            let Some(package) = self.root.join(rel).parent().map(normalize) else {
                continue;
            };
            if !package.starts_with(&self.root) {
                continue;
            }
            for dir in dirs {
                let dir = normalize(&package.join(dir));
                if dir.starts_with(&package) && !lean_roots.contains(&dir) {
                    lean_roots.push(dir);
                }
            }
            if !lean_roots.contains(&package) {
                lean_roots.push(package);
            }
        }
        self.lean_roots = (!lean_roots.is_empty()).then_some(lean_roots);
        let mut js_packages = HashMap::new();
        for (rel, package) in &manifests.js {
            let Some(dir) = self.root.join(rel).parent().map(normalize) else {
                continue;
            };
            if dir.starts_with(&self.root) {
                // The shallowest of two of one name: a fixture's copy of
                // a package is not the package.
                js_packages
                    .entry(package.name.clone())
                    .or_insert((dir, package.exports.clone()));
            }
        }
        self.js_packages = js_packages;
        self
    }

    /// [`Resolver::new`] with the go.mod / pubspec metadata supplied by the
    /// caller instead of read from disk — for REMOTE projects, whose root
    /// names a path on another host: the metadata arrives with the server's
    /// project snapshot, and nothing here may touch the local filesystem.
    pub fn with_meta(
        root: &Path,
        files: &[PathBuf],
        go_module: Option<String>,
        dart_package: Option<String>,
    ) -> Self {
        let file_set: HashSet<PathBuf> = files.iter().cloned().collect();
        let mut dirs = HashSet::new();
        for f in files {
            let mut p = f.parent();
            while let Some(d) = p {
                if !dirs.insert(d.to_path_buf()) {
                    break; // ancestors already recorded
                }
                if d == root {
                    break;
                }
                p = d.parent();
            }
        }
        // Every crate root in the project — a workspace has one per member,
        // and one package can have several targets. Cargo's auto-discovered
        // layouts each make their own crate root, so a file under them
        // resolves `crate::` to itself, not to the package's lib:
        //   src/lib.rs, src/main.rs                       (lib / default bin)
        //   src/bin/<name>.rs, src/bin/<name>/main.rs     (extra bins)
        //   examples|tests|benches/<name>.rs, …/<name>/main.rs
        let mut rust_crate_roots: Vec<PathBuf> = file_set
            .iter()
            .filter(|f| is_rust_crate_root(f, &file_set))
            .cloned()
            .collect();
        rust_crate_roots.sort();
        rust_crate_roots.dedup();
        let mut packages: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for file in file_set.iter().filter(|f| is_package_source(f)) {
            if let Some(dir) = file.parent() {
                packages
                    .entry(dir.to_path_buf())
                    .or_default()
                    .push(file.clone());
            }
        }
        for files in packages.values_mut() {
            files.sort();
        }
        let mut java_roots: Vec<PathBuf> = dirs
            .iter()
            .filter(|d| {
                ["main", "test"].iter().any(|set| {
                    ["java", "kotlin", "scala"]
                        .iter()
                        .any(|lang| d.ends_with(Path::new("src").join(set).join(lang)))
                })
            })
            .cloned()
            .collect();
        java_roots.sort();
        let mut erlang_modules: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut erlang_headers: HashMap<String, Vec<PathBuf>> = HashMap::new();
        let mut erlang_apps: HashMap<String, PathBuf> = HashMap::new();
        let mut c_headers: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for file in &file_set {
            let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if file
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| C_INCLUDABLE.contains(&e))
            {
                c_headers
                    .entry(name.to_string())
                    .or_default()
                    .push(file.clone());
            }
            if let Some(module) = name.strip_suffix(".erl") {
                erlang_modules
                    .entry(module.to_string())
                    .or_default()
                    .push(file.clone());
            } else if name.ends_with(".hrl") {
                erlang_headers
                    .entry(name.to_string())
                    .or_default()
                    .push(file.clone());
            } else if let Some(app) = name.strip_suffix(".app.src")
                && let Some(src) = file.parent().filter(|d| d.ends_with("src"))
                && let Some(dir) = src.parent()
            {
                // Two of one name: the shallower is the application's own.
                let shallower =
                    |held: &PathBuf| held.components().count() <= dir.components().count();
                if !erlang_apps.get(app).is_some_and(shallower) {
                    erlang_apps.insert(app.to_string(), dir.to_path_buf());
                }
            }
        }
        for files in erlang_modules
            .values_mut()
            .chain(erlang_headers.values_mut())
            .chain(c_headers.values_mut())
        {
            files.sort();
        }
        let haskell_roots = haskell_roots(root, &[], &dirs);
        let mut lean_modules: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for file in file_set
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "lean"))
        {
            let Ok(rel) = file.strip_prefix(root) else {
                continue;
            };
            let parts: Vec<String> = rel
                .with_extension("")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            for start in 0..parts.len() {
                lean_modules
                    .entry(parts[start..].join("."))
                    .or_default()
                    .push(file.clone());
            }
        }
        for files in lean_modules.values_mut() {
            files.sort();
        }
        Resolver {
            root: root.to_path_buf(),
            files: file_set,
            dirs,
            rust_crate_roots,
            go_module,
            dart_package,
            ts_configs: Arc::default(),
            packages: Arc::new(packages),
            lean_modules: Arc::new(lean_modules),
            lean_roots: None,
            java_roots,
            php_psr4: Vec::new(),
            haskell_roots,
            rust_libs: HashMap::new(),
            zig_modules: Vec::new(),
            dart_packages: HashMap::new(),
            python_roots: Vec::new(),
            erlang_modules,
            erlang_headers,
            erlang_apps,
            c_headers,
            js_packages: HashMap::new(),
        }
    }

    /// The directory `crate::` resolves against for `from`: the module
    /// directory of the innermost crate root that owns it. In a workspace
    /// each member resolves against its own crate, never a sibling's; within
    /// a package, `src/bin/x.rs` resolves against `src/bin/`, not `src/`.
    fn rust_crate_src_for(&self, from: &Path) -> Option<PathBuf> {
        self.rust_crate_roots
            .iter()
            .filter(|r| {
                // The crate root file itself, or anything under its modules.
                *r == from || from.starts_with(self.rust_mod_dir(r))
            })
            .map(|r| self.rust_mod_dir(r))
            .max_by_key(|d| d.components().count())
    }

    fn has_file(&self, p: &Path) -> bool {
        self.files.contains(p)
    }

    /// Resolve one raw import from `from` (the importing file), with no
    /// knowledge of other files' imports (Rust re-exports are not followed;
    /// see [`Resolver::resolve_with`]) or of what files declare (no Java
    /// `package` line, no C# namespace). Test-only: the graph always resolves
    /// with both.
    #[cfg(test)]
    pub fn resolve(&self, raw: &RawImport, from: &Path, lang: &str) -> Target {
        self.resolve_with(raw, from, lang, &RustCtx::default(), &Declared::default())
    }

    /// Resolve one raw import from `from`, using `ctx` (built over the whole
    /// file set) to follow Rust re-exports and local-module paths, and
    /// `declared` for what the importer and the other files declare
    /// themselves part of (a Java package, C# namespaces).
    pub fn resolve_with(
        &self,
        raw: &RawImport,
        from: &Path,
        lang: &str,
        ctx: &RustCtx,
        declared: &Declared<'_>,
    ) -> Target {
        match lang {
            "rust" => self.resolve_rust(raw, from, &Consulted::new(ctx)),
            "python" => self.resolve_python(raw, from),
            "javascript" | "typescript" | "tsx" | "vue" | "svelte" | "astro" => {
                self.resolve_js(raw, from)
            }
            "go" => self.resolve_go(raw),
            "dart" => self.resolve_dart(raw, from),
            "lean" => self.resolve_lean(raw, from),
            // Kotlin's and Scala's packages and classes are Java's.
            "java" | "kotlin" | "scala" => self.resolve_java(raw, from, declared),
            "elixir" => self.resolve_elixir(raw, declared),
            "csharp" => self.resolve_csharp(raw, from, declared.namespaces),
            "php" => self.resolve_php(raw, from),
            "ruby" => self.resolve_ruby(raw, from),
            "erlang" => self.resolve_erlang(raw, from),
            "haskell" => self.resolve_haskell(raw, from),
            "zig" => self.resolve_zig(raw, from),
            "c" | "cpp" => self.resolve_c(raw, from),
            "html" | "css" | "scss" | "less" => self.resolve_web(raw, from, lang),
            _ => Target::External(raw.module.clone()),
        }
    }

    // --- Pages and stylesheets ---

    /// What a page or a stylesheet loads, as the browser, Vite and Sass
    /// read it. A URL of the web (`https://`, `//cdn…`) is that host's, and
    /// a Sass built-in (`sass:math`) is Sass's. `/src/main.ts` is from the
    /// page's folder — Vite's root — or a folder above it in the project,
    /// or their `public/` (`/main.css`).
    /// Anything else is beside the importer first (`@import "variables"`
    /// is), with Sass's and Less's extensions, partials (`_variables.scss`)
    /// and index files (`_index.scss`); then, unprefixed, an alias the
    /// scripts' resolution knows (`@/styles/x`, a tsconfig path); a
    /// package's path (`bootstrap/scss/bootstrap`, webpack's `~bootstrap`)
    /// is that package's.
    fn resolve_web(&self, raw: &RawImport, from: &Path, lang: &str) -> Target {
        let spec = raw.module.split(['?', '#']).next().unwrap_or("").trim();
        if spec.is_empty() {
            return Target::Unresolved(raw.module.clone());
        }
        if let Some(rest) = ["https://", "http://", "//"]
            .iter()
            .find_map(|p| spec.strip_prefix(p))
        {
            return Target::External(rest.split('/').next().unwrap_or(rest).to_string());
        }
        if spec.contains(':') {
            return Target::External(spec.to_string());
        }
        let dir = from.parent().unwrap_or(&self.root);
        if let Some(rest) = spec.strip_prefix('/') {
            // From the page's folder up — and its `public/`, which Vite,
            // Create React App, Next.js and Nuxt serve at `/`.
            let mut at = Some(dir);
            while let Some(d) = at.filter(|d| d.starts_with(&self.root)) {
                for base in [d.to_path_buf(), d.join("public")] {
                    if let Some(found) = self.web_file(&normalize(&base.join(rest)), lang) {
                        return Target::Internal(found);
                    }
                }
                at = d.parent();
            }
            return Target::Unresolved(spec.to_string());
        }
        if let Some(package) = spec.strip_prefix('~').filter(|p| !p.starts_with('/')) {
            return Target::External(js_package(package));
        }
        if let Some(found) = self.web_file(&normalize(&dir.join(spec)), lang) {
            return Target::Internal(found);
        }
        if spec.starts_with('.') {
            return Target::Unresolved(spec.to_string());
        }
        if let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/"))
            && let Some(found) = ["src", "app", ""]
                .iter()
                .find_map(|d| self.web_file(&normalize(&self.root.join(d).join(rest)), lang))
        {
            return Target::Internal(found);
        }
        match self.resolve_js(raw, from) {
            Target::Internal(found) => Target::Internal(found),
            _ => Target::External(js_package(spec)),
        }
    }

    /// The file a page's or stylesheet's path names: as written, else with
    /// the extensions, partials and index files its language reads.
    fn web_file(&self, path: &Path, lang: &str) -> Option<PathBuf> {
        if self.files.contains(path) {
            return Some(path.to_path_buf());
        }
        let extensions: &[&str] = match lang {
            "scss" => &["scss", "sass", "css"],
            "less" => &["less", "css"],
            "css" => &["css"],
            _ => &[],
        };
        let name = path.file_name()?.to_str()?;
        let parent = path.parent()?;
        // `@use "x.scss"` of `_x.scss`.
        if lang == "scss" {
            let partial = parent.join(format!("_{name}"));
            if self.files.contains(&partial) {
                return Some(partial);
            }
        }
        extensions.iter().find_map(|ext| {
            [
                parent.join(format!("{name}.{ext}")),
                parent.join(format!("_{name}.{ext}")),
                path.join(format!("_index.{ext}")),
                path.join(format!("index.{ext}")),
            ]
            .into_iter()
            .find(|candidate| self.files.contains(candidate))
        })
    }

    // --- Java ---

    /// A Java import names a class by its package's path under a source
    /// root: the importer's own first — its directory less one level per part
    /// of the package its `package` line declares — then every
    /// `src/main/java` and `src/test/java` of the project, its own module's
    /// first (a test sees its module's main classes). `a.b.C` is
    /// `<root>/a/b/C.java`; a static member `a.b.C.m` and a nested class
    /// `a.b.C.D` are found through `C`'s file, the longest prefix that names
    /// one. `a.b.*` names the package: its directory, which reaches the
    /// package's classes as a Go package's directory does
    /// ([`ImportGraph::packages`]). A name in a package of the project's own
    /// that no file is stays unresolved; any other is a library's, named by
    /// its package (`java.util`).
    fn resolve_java(&self, raw: &RawImport, from: &Path, declared: &Declared<'_>) -> Target {
        let package = declared.package;
        let spec = raw.module.as_str();
        let (name, star) = match spec.strip_suffix(".*") {
            Some(name) => (name, true),
            None => (spec, false),
        };
        let segs: Vec<&str> = name.split('.').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return Target::Unresolved(spec.to_string());
        }
        let own = self.java_own_root(from, package);
        let roots = self.java_roots_for(from, own.as_ref());
        // A package filed under one root may have files under others too —
        // `src/main/kotlin` and `src/test/kotlin`, another module's — so a
        // star import or a top-level member reaches the package's node,
        // every file declaring it, where any is outside the folder found.
        let package_target = |segs: &[&str], dir: PathBuf| -> Target {
            let node = self.package_node(&segs.join("."));
            // The files are sorted, so those under `dir` are one run: found
            // by two binary searches, not a scan per import.
            let outside = |files: &[PathBuf]| {
                let start = files.partition_point(|f| f.as_path() < dir.as_path());
                let end =
                    files.partition_point(|f| f.as_path() < dir.as_path() || f.starts_with(&dir));
                end - start != files.len()
            };
            match declared
                .namespaces
                .and_then(|namespaces| namespaces.files(&node))
                .is_some_and(outside)
            {
                true => Target::Internal(node),
                false => Target::Internal(dir),
            }
        };
        for root in &roots {
            // A root without the name's first package is passed over in one
            // lookup: most imports are a library's, tried under every root.
            if segs.len() > 1 && !self.dirs.contains(&root.join(segs[0])) {
                continue;
            }
            if star {
                let dir = join_all(root, &segs);
                if self.packages.contains_key(&dir) {
                    return package_target(&segs, dir);
                }
            }
            // The importer's own class (a static import of its own members)
            // is found too: the graph drops it as no dependency. A Kotlin
            // or Scala class is its file's, as a Java one is.
            for end in (1..=segs.len()).rev() {
                let (class, dirs) = segs[..end].split_last().expect("one segment at least");
                for ext in ["java", "kt", "scala"] {
                    let file = join_all(root, dirs).join(format!("{class}.{ext}"));
                    if self.has_file(&file) {
                        return Target::Internal(file);
                    }
                }
            }
            // A Kotlin top-level function or property (`import a.b.format`),
            // or a Scala package object's member: its package's.
            if !star
                && segs.len() > 1
                && segs
                    .last()
                    .is_some_and(|s| s.starts_with(char::is_lowercase))
            {
                let dir = join_all(root, &segs[..segs.len() - 1]);
                if self.packages.contains_key(&dir) {
                    return package_target(&segs[..segs.len() - 1], dir);
                }
            }
        }
        // The package is the name up to its first capitalised part, a class
        // — or, with none, up to a Kotlin top-level function's own name.
        let package_len = segs
            .iter()
            .position(|s| s.starts_with(char::is_uppercase))
            .unwrap_or(match star {
                true => segs.len(),
                false => segs.len().saturating_sub(1),
            })
            .max(1);
        let package = &segs[..package_len];
        let filed = roots
            .iter()
            .any(|root| self.dirs.contains(&join_all(root, package)));
        // A package the project's files declare wherever they are: its node,
        // which reaches them — the longest of the name's prefixes that is
        // one. Kotlin and Scala put a package's files anywhere, a class in
        // any file of it; a Java class is its own file, so a Java package
        // filed under its name with no file of the class is missing it.
        if let Some(namespaces) = declared.namespaces {
            for end in (1..=segs.len()).rev() {
                let node = self.package_node(&segs[..end].join("."));
                let Some(files) = namespaces.files(&node).filter(|f| !f.is_empty()) else {
                    continue;
                };
                let java_only = files
                    .iter()
                    .all(|f| f.extension().is_some_and(|e| e == "java"));
                if !(filed && java_only) {
                    return Target::Internal(node);
                }
            }
        }
        if filed {
            return Target::Unresolved(spec.to_string());
        }
        Target::External(package.join("."))
    }

    /// Where a Java file's `package` line says its source root is: its
    /// directory less one level per part of the package — when the directory
    /// does end in them; a file filed elsewhere says nothing of where its
    /// root is. No line: the default package, rooted where the file is.
    fn java_own_root(&self, from: &Path, package: Option<&str>) -> Option<PathBuf> {
        let mut root = from.parent().unwrap_or(&self.root).to_path_buf();
        let Some(package) = package else {
            return Some(root);
        };
        package
            .split('.')
            .rev()
            .all(|part| root.file_name().is_some_and(|n| n == part) && root.pop())
            .then_some(root)
    }

    /// The source roots a Java file's imports are looked up under, in order
    /// (see [`Resolver::resolve_java`]): its `own`, then the roots of the
    /// module it is in — the innermost when modules nest — then every other
    /// module's in path order. Borrowed, and only the importer's own few
    /// sorted: this runs once per import, over every module's roots.
    fn java_roots_for<'a>(&'a self, from: &Path, own: Option<&'a PathBuf>) -> Vec<&'a PathBuf> {
        let in_module = |r: &PathBuf| r.ancestors().nth(3).is_some_and(|m| from.starts_with(m));
        let mut own_module: Vec<&PathBuf> =
            self.java_roots.iter().filter(|r| in_module(r)).collect();
        own_module.sort_by_key(|r| std::cmp::Reverse(r.components().count()));
        let others = self.java_roots.iter().filter(|r| !in_module(r));
        let conventional = own_module.into_iter().chain(others);
        own.into_iter()
            .chain(conventional.filter(|r| Some(*r) != own))
            .collect()
    }

    // --- C# ---

    /// A C# `using A.B;` names a namespace, which is no file: whatever files
    /// declare it, wherever they are ([`Namespaces`]). It resolves to the
    /// namespace's own node ([`Resolver::namespace_node`]), which reaches
    /// those files as a Go package's directory reaches its own. A `using
    /// static A.B.C;` — or an alias of a type — names a type: the file of the
    /// namespace `A.B` named after it, by C#'s one-type-a-file convention,
    /// else the namespace. A name no project file declares is a library's.
    fn resolve_csharp(
        &self,
        raw: &RawImport,
        from: &Path,
        namespaces: Option<&Namespaces>,
    ) -> Target {
        let name = raw.module.as_str();
        let files_of = |namespace: &str| {
            let node = self.namespace_node(namespace);
            namespaces
                .and_then(|n| n.files(&node))
                .map(|files| (node, files))
        };
        if let Some((node, _)) = files_of(name) {
            return Target::Internal(node);
        }
        if let Some((namespace, ty)) = name.rsplit_once('.')
            && let Some((node, files)) = files_of(namespace)
        {
            let file = files
                .iter()
                .find(|f| f.as_path() != from && f.file_stem().is_some_and(|s| s == ty));
            return Target::Internal(file.cloned().unwrap_or(node));
        }
        Target::External(name.to_string())
    }

    /// The node C# namespace `namespace` is in the graph: `<root>/namespace
    /// A.B`, which the Imports tree shows as `namespace A.B` — and which no
    /// project file or folder is called, where a folder named after a
    /// namespace (`Acme.Core/`, a project's) is common.
    pub fn namespace_node(&self, namespace: &str) -> PathBuf {
        self.root.join(format!("namespace {namespace}"))
    }

    /// The node a Java, Kotlin or Scala package is in the graph when its
    /// files are not filed under its name (`<root>/package os`): Kotlin and
    /// Scala put a package's files wherever they like.
    pub fn package_node(&self, package: &str) -> PathBuf {
        self.root.join(format!("package {package}"))
    }

    // --- PHP ---

    /// A PHP name (`use A\B\C`) is a class file by PSR-4: the project's
    /// `composer.json` files map namespace prefixes to directories, the
    /// longest prefix first (`App\` → `app/`: `App\Models\User` is
    /// `app/Models/User.php`); with none that maps it, the name's own path
    /// from the project root. A name under a mapped prefix that no file is
    /// stays unresolved; any other is a package's, named by its vendor's
    /// namespace (`Symfony`). A `require`/`include` path is a file relative
    /// to the importer (`__DIR__` is its directory), else to the project root.
    fn resolve_php(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        let dir = from.parent().unwrap_or(&self.root);
        let found = |file: PathBuf| self.has_file(&file).then_some(file);
        if let Some(rest) = spec.strip_prefix("__DIR__") {
            return found(normalize(&dir.join(rest.trim_start_matches('/'))))
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(spec.to_string()));
        }
        // A class name has neither; a path has one or both.
        if spec.contains('/') || spec.contains('.') {
            if spec.starts_with('/') {
                return Target::Unresolved(spec.to_string());
            }
            return [dir, self.root.as_path()]
                .into_iter()
                .find_map(|base| found(normalize(&base.join(spec))))
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(spec.to_string()));
        }
        let mut mapped = false;
        for (prefix, dirs) in &self.php_psr4 {
            let Some(rest) = spec.strip_prefix(prefix.as_str()).filter(|r| !r.is_empty()) else {
                continue;
            };
            mapped |= !prefix.is_empty();
            let rel = format!("{}.php", rest.replace('\\', "/"));
            if let Some(file) = dirs.iter().find_map(|d| found(normalize(&d.join(&rel)))) {
                return Target::Internal(file);
            }
        }
        if mapped {
            return Target::Unresolved(spec.to_string());
        }
        let rel = format!("{}.php", spec.replace('\\', "/"));
        if let Some(file) = found(self.root.join(rel)) {
            return Target::Internal(file);
        }
        Target::External(spec.split('\\').next().unwrap_or(spec).to_string())
    }

    // --- Ruby ---

    /// A `require_relative` (recorded `./x`) is a file relative to the
    /// importer. A `require` or `load` names a feature on the load path: the
    /// importer's own `lib` root (a gem's, in a repository of several), the
    /// project's `lib/`, then `spec/` and `test/`, which RSpec and Minitest
    /// put on it for their helpers. A feature none of them has is a gem's,
    /// named by its first part (`active_support/all` is `active_support`).
    fn resolve_ruby(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        if spec.starts_with('/') {
            return Target::Unresolved(spec.to_string());
        }
        if spec.starts_with("./") || spec.starts_with("../") {
            let dir = from.parent().unwrap_or(&self.root);
            return self
                .ruby_file(&normalize(&dir.join(spec)))
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(spec.to_string()));
        }
        let mut roots: Vec<PathBuf> = Vec::new();
        for name in ["lib", "spec", "test"] {
            if let Some(own) = self.enclosing_dir_named(from, &[name]) {
                roots.push(own.to_path_buf());
            }
            roots.push(self.root.join(name));
        }
        roots
            .iter()
            .find_map(|root| self.ruby_file(&normalize(&root.join(spec))))
            .map(Target::Internal)
            .unwrap_or_else(|| Target::External(spec.split('/').next().unwrap_or(spec).to_string()))
    }

    /// The file a Ruby feature path names: `x` is `x.rb`; one that already
    /// names its file (`load "x.rb"`) is itself.
    fn ruby_file(&self, path: &Path) -> Option<PathBuf> {
        [with_added_ext(path, "rb"), path.to_path_buf()]
            .into_iter()
            .find(|f| self.has_file(f))
    }

    /// The nearest directory above `from`, within the project, named one of
    /// `names`.
    fn enclosing_dir_named<'p>(&self, from: &'p Path, names: &[&str]) -> Option<&'p Path> {
        from.ancestors()
            .skip(1)
            .take_while(|d| d.starts_with(&self.root))
            .find(|d| {
                d.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| names.contains(&n))
            })
    }

    // --- Erlang ---

    /// `-behaviour(m)` and `-import(m, …)` name a module: the project's
    /// `m.erl` wherever it is (the one nearest the importer, when several
    /// are), else a library's (`gen_server`, OTP's). An `-include` path is a
    /// header relative to the importer, then in its application's `include/`
    /// (beside the `src/` or `test/` it is in) or from the application's own
    /// directory, then in the project's `include/` or from its root. An
    /// `-include_lib("app/include/x.hrl")` is the header in application
    /// `app` wherever the project keeps it — the directory of its
    /// `src/app.app.src`, or any directory named `app` (`apps/app`,
    /// `deps/app`, `_build/default/lib/app`) — else, as Erlang does, where an
    /// `-include` would find it, else in that application outside the project.
    /// An Elixir module by Mix's convention: `Shop.Pricing.Tax` is
    /// `shop/pricing/tax.ex` under a `lib/` — the project's, or an umbrella
    /// app's — or `test/support/`; a module found in none is its library's
    /// (`Ecto.Query` is Ecto's, `GenServer` Elixir's own).
    fn resolve_elixir(&self, raw: &RawImport, declared: &Declared<'_>) -> Target {
        let spec = raw.module.as_str();
        let parts: Vec<String> = spec.split('.').map(snake_case).collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Target::Unresolved(spec.to_string());
        }
        let rel: PathBuf = parts.iter().collect::<PathBuf>().with_extension("ex");
        let mut roots: Vec<&PathBuf> = self
            .dirs
            .iter()
            .filter(|d| d.ends_with("lib") || d.ends_with("test/support"))
            .collect();
        roots.sort();
        roots
            .into_iter()
            .map(|root| root.join(&rel))
            .find(|file| self.has_file(file))
            // Not where Mix's convention puts it (`HexpmWeb` in
            // `lib/hexpm_web/web.ex`, `Hexpm.OAuth` under `oauth/`): the
            // file that defines it.
            .or_else(|| {
                declared
                    .namespaces?
                    .files(&self.package_node(spec))?
                    .first()
                    .cloned()
            })
            .map(Target::Internal)
            .unwrap_or_else(|| Target::External(spec.split('.').next().unwrap_or(spec).to_string()))
    }

    fn resolve_erlang(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        if let Some(path) = spec.strip_prefix(ERLANG_LIB) {
            let app_header = path.split_once('/').and_then(|(app, rest)| {
                let in_app = self.erlang_apps.get(app).map(|dir| dir.join(rest));
                let header = Path::new(path).file_name()?.to_str()?;
                let named = self.erlang_headers.get(header).and_then(|files| {
                    nearest(
                        files
                            .iter()
                            .filter(|f| f.ends_with(path) && f.as_path() != from),
                        from,
                    )
                });
                in_app
                    .filter(|f| self.has_file(f))
                    .or_else(|| named.cloned())
            });
            return match app_header.or_else(|| self.erlang_include(path, from)) {
                Some(file) => Target::Internal(file),
                None => match path.split_once('/') {
                    Some((app, _)) => Target::External(app.to_string()),
                    None => Target::Unresolved(path.to_string()),
                },
            };
        }
        // A header's path; a module's name has no `/` and is no `.hrl`.
        if spec.contains('/') || spec.ends_with(".hrl") {
            return self
                .erlang_include(spec, from)
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(spec.to_string()));
        }
        self.erlang_modules
            .get(spec)
            .and_then(|files| nearest(files.iter().filter(|f| f.as_path() != from), from))
            .map(|file| Target::Internal(file.clone()))
            .unwrap_or_else(|| Target::External(spec.to_string()))
    }

    /// Where an Erlang `-include` of `path` finds its header from `from`
    /// (see [`Resolver::resolve_erlang`]).
    fn erlang_include(&self, path: &str, from: &Path) -> Option<PathBuf> {
        if path.starts_with('/') {
            return None;
        }
        let mut bases: Vec<PathBuf> = vec![from.parent().unwrap_or(&self.root).to_path_buf()];
        if let Some(app) = self
            .enclosing_dir_named(from, &["src", "test"])
            .and_then(Path::parent)
        {
            bases.push(app.join("include"));
            bases.push(app.to_path_buf());
        }
        bases.push(self.root.join("include"));
        bases.push(self.root.clone());
        bases
            .into_iter()
            .map(|base| normalize(&base.join(path)))
            .find(|f| f != from && self.has_file(f))
    }

    // --- Haskell ---

    /// `import A.B.C` names the file `A/B/C.hs` (or `.lhs`) under a source
    /// root: the directories the project's `.cabal` files (`hs-source-dirs`)
    /// and `package.yaml` files (`source-dirs`) name, then the conventional
    /// `src`, `app`, `lib` and `test` of each package and of the project, and
    /// the package's own directory — the roots holding the importer first,
    /// the deepest first. A module no project file is is a package's
    /// (`Data.Map` is `containers`'), named by itself.
    fn resolve_haskell(&self, raw: &RawImport, from: &Path) -> Target {
        let module = raw.module.as_str();
        let rel = module.replace('.', "/");
        let mut roots: Vec<&PathBuf> = self.haskell_roots.iter().collect();
        roots.sort_by_key(|r| {
            std::cmp::Reverse(if from.starts_with(r) {
                r.components().count()
            } else {
                0
            })
        });
        roots
            .into_iter()
            .flat_map(|root| ["hs", "lhs"].map(|ext| root.join(format!("{rel}.{ext}"))))
            .find(|f| f != from && self.has_file(f))
            .map(Target::Internal)
            .unwrap_or_else(|| Target::External(module.to_string()))
    }

    // --- Zig ---

    /// A Zig `@import` of a path (`x.zig`, `../y.zig`, a `.zon`) is the file
    /// relative to the importer; a bare name the importer's `build.zig`
    /// gives one of the project's modules (`@import("greet")`) is that
    /// module's root file; any other — `std`, `builtin`, `root`, a
    /// `build.zig.zon` dependency's module — is a package.
    fn resolve_zig(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        let is_path = spec.contains('/') || spec.ends_with(".zig") || spec.ends_with(".zon");
        if !is_path {
            let declared = self
                .zig_modules
                .iter()
                .find(|(dir, _)| from.starts_with(dir))
                .and_then(|(_, named)| named.get(spec));
            return match declared {
                Some(file) => Target::Internal(file.clone()),
                None => Target::External(spec.to_string()),
            };
        }
        let file = normalize(&from.parent().unwrap_or(&self.root).join(spec));
        if !spec.starts_with('/') && self.has_file(&file) {
            return Target::Internal(file);
        }
        Target::Unresolved(spec.to_string())
    }

    // --- C and C++ ---

    /// An `#include "cart.hpp"` is the file beside the includer first, as
    /// the preprocessor looks; then, as an `#include <shop/cart.hpp>` is, a
    /// project file the include path reaches — one whose path ends in the
    /// include's (under `include/`, `src/`, a library's folder: wherever a
    /// `-I` points, the path's end finds it), of several the one nearest
    /// the includer. A header that is no project file is a library's: the
    /// C++ standard library's (`<vector>`), the C library's (`<stdio.h>`),
    /// POSIX's (`<unistd.h>`, `<sys/stat.h>`), or a package's by its first
    /// folder or name (`<boost/asio.hpp>` is `boost`, `<zlib.h>` `zlib`).
    fn resolve_c(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.trim();
        let (angle, path) = match spec.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            Some(inner) => (true, inner.trim()),
            None => (false, spec),
        };
        let path = path.strip_prefix("./").unwrap_or(path);
        if path.is_empty() {
            return Target::Unresolved(spec.to_string());
        }
        if !angle && !path.starts_with('/') {
            let beside = normalize(&from.parent().unwrap_or(&self.root).join(path));
            if self.has_file(&beside) {
                return Target::Internal(beside);
            }
        }
        let rel = Path::new(path);
        let shared = |f: &Path| {
            f.components()
                .zip(from.components())
                .take_while(|(a, b)| a == b)
                .count()
        };
        let found = rel
            .is_relative()
            .then(|| rel.file_name().and_then(|n| n.to_str()))
            .flatten()
            .and_then(|name| self.c_headers.get(name))
            .and_then(|files| {
                files
                    .iter()
                    .filter(|f| f.as_path() != from && f.ends_with(rel))
                    .max_by_key(|f| (shared(f), std::cmp::Reverse(f.components().count())))
            });
        match found {
            Some(file) => Target::Internal(file.clone()),
            None => Target::External(c_library(path)),
        }
    }

    // --- Lean ---

    /// `import A.B.C` names the file `A/B/C.lean` under a source root of the
    /// importer's package — the project's root, its `src/`, a library's own
    /// directory in a monorepo. Of the files that can be that module, the one
    /// whose root holds the importer (the deepest, when several do) is it.
    /// A module no project file can be is another package's: the toolchain's
    /// (`Init`, `Std`, `Lean`) or a dependency's (`Mathlib`), named by its
    /// first part.
    fn resolve_lean(&self, raw: &RawImport, from: &Path) -> Target {
        let module = raw.module.as_str();
        let Some(files) = self.lean_modules.get(module) else {
            let package = module.split('.').next().unwrap_or(module);
            return Target::External(package.to_string());
        };
        let depth = module.split('.').count();
        let source_root = |file: &Path| {
            let mut root = file.with_extension("");
            for _ in 0..depth {
                root.pop();
            }
            root
        };
        // Where the project's lakefiles say modules are found: a file found
        // under another folder is no module of that name (`MyLib/Lean/Meta`
        // is `MyLib.Lean.Meta`, never core's `Lean.Meta`).
        let candidates: Vec<(PathBuf, &PathBuf)> = files
            .iter()
            .filter(|f| f.as_path() != from)
            .map(|f| (source_root(f), f))
            .filter(|(root, _)| {
                self.lean_roots
                    .as_ref()
                    .is_none_or(|roots| roots.contains(root))
            })
            .collect();
        if candidates.is_empty() && self.lean_roots.is_some() {
            let package = module.split('.').next().unwrap_or(module);
            return Target::External(package.to_string());
        }
        // Under a root holding the importer, the deepest; else — with
        // lakefiles to say so — under one of the importer's own package (a
        // `srcDir` library a root `Main.lean` imports), else the one of
        // another package of the project (`require foo from "../foo"`).
        let holding = candidates
            .iter()
            .filter(|(root, _)| from.starts_with(root))
            .max_by_key(|(root, _)| root.components().count());
        let found = holding.or_else(|| {
            let roots = self.lean_roots.as_ref()?;
            let package = roots
                .iter()
                .filter(|r| from.starts_with(r))
                .max_by_key(|r| r.components().count())?;
            candidates
                .iter()
                .find(|(root, _)| root.starts_with(package))
                .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
        });
        found.map_or_else(
            || Target::Unresolved(module.to_string()),
            |(_, f)| Target::Internal((*f).clone()),
        )
    }

    // --- Dart ---

    fn resolve_dart(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        // `dart:core`, `dart:async` — the SDK, always external.
        if let Some(rest) = spec.strip_prefix("dart:") {
            return Target::External(format!("dart:{}", rest.split('/').next().unwrap_or(rest)));
        }
        // `package:<name>/<path>` — another package, except one of the
        // project's: its own (the root's pubspec) or a workspace's other
        // package by its pubspec's name, whose `lib/<path>` it names.
        if let Some(rest) = spec.strip_prefix("package:") {
            let (pkg, path) = rest.split_once('/').unwrap_or((rest, ""));
            if !path.is_empty() {
                let lib = match self.dart_package.as_deref() == Some(pkg) {
                    true => Some(self.root.join("lib")),
                    false => self.dart_packages.get(pkg).cloned(),
                };
                if let Some(lib) = lib {
                    let cand = normalize(&lib.join(path));
                    if self.has_file(&cand) {
                        return Target::Internal(cand);
                    }
                }
            }
            return Target::External(format!("package:{pkg}"));
        }
        // Anything else is a path relative to the importing file (`src/foo.dart`,
        // `../bar.dart`) — Dart relative URIs always name a `.dart` file directly.
        let base = from.parent().unwrap_or(&self.root);
        let joined = normalize(&base.join(spec));
        if self.has_file(&joined) {
            return Target::Internal(joined);
        }
        Target::Unresolved(spec.to_string())
    }

    // --- Rust ---

    fn resolve_rust(&self, raw: &RawImport, from: &Path, ctx: &Consulted<'_>) -> Target {
        self.resolve_rust_inferred(raw, from, ctx).0
    }

    /// [`Resolver::resolve_rust`], and whether the answer was reached only by
    /// elimination (see [`EdgeKind::Inferred`]).
    fn resolve_rust_inferred(
        &self,
        raw: &RawImport,
        from: &Path,
        ctx: &Consulted<'_>,
    ) -> (Target, bool) {
        if raw.is_mod_decl {
            // Submodules live beside a `mod.rs` or any crate root, or
            // in a directory named after a plain module file. `a::b` is a `mod b;`
            // written inside inline `mod a { … }` (see
            // `clew_core::rustscope::scope_rust_imports`): its file lives one
            // directory further down, `a/b.rs`.
            let dir = self.rust_mod_dir(from);
            let (name, path) = clew_core::rustscope::mod_decl_parts(&raw.module);
            let segs = rust_path_segments(name);
            // `#[path = "…"]` names the file: from the declaring file's
            // folder — or, inside inline modules, from the module's own
            // folder and theirs below it, as rustc finds it.
            if let Some(path) = path {
                let base = match segs.split_last() {
                    Some((_, inline)) if !inline.is_empty() => {
                        inline.iter().fold(dir.clone(), |d, s| d.join(s))
                    }
                    _ => from.parent().unwrap_or(&self.root).to_path_buf(),
                };
                let file = normalize(&base.join(path));
                let target = match self.has_file(&file) {
                    true => Target::Internal(file),
                    false => Target::Unresolved(raw.module.clone()),
                };
                return (target, false);
            }
            let target = self
                .rust_child_module_file(from, &dir, &segs)
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(raw.module.clone()));
            return (target, false);
        }
        // A glob resolves to the module it reads from; the re-export marker
        // does not change where the path points.
        self.rust_path_inferred(RustUse::parse(&raw.module).path, from, ctx, 0)
    }

    /// Resolve a `use` path written in `from` (paths inside inline modules are
    /// already rewritten to be relative to the file; see
    /// [`clew_core::rustscope::scope_rust_imports`]), and say whether the
    /// answer is the fallback of [`Resolver::rust_resolve_in`] — nothing but
    /// elimination says the name lives there. A path naming `from` itself (or
    /// an item in it) resolves to `Internal(from)`, which the graph drops as a
    /// self-edge.
    fn rust_path_inferred(
        &self,
        path: &str,
        from: &Path,
        ctx: &Consulted<'_>,
        hops: usize,
    ) -> (Target, bool) {
        let segs = rust_path_segments(path);
        let Some((&head, rest)) = segs.split_first() else {
            return (Target::Unresolved(path.to_string()), false);
        };
        let (base, rest): (PathBuf, &[&str]) = match head {
            "crate" => match self.rust_crate_src_for(from) {
                Some(d) => (d, rest),
                None => return (Target::External(path.to_string()), false),
            },
            // `super::x` → a sibling of the current module: one level up from
            // the current module's directory. For a plain file `dir/name.rs`
            // that's `dir` (= `from.parent()`), but for `dir/mod.rs` the module
            // dir is `dir`, so `super` must be `dir`'s parent — hence go via
            // `rust_mod_dir`. Every further `super` climbs one more level.
            "super" => {
                let mut dir = self.rust_mod_dir(from);
                let mut rest = rest;
                loop {
                    dir = dir.parent().unwrap_or(&self.root).to_path_buf();
                    match rest.split_first() {
                        Some((&"super", more)) => rest = more,
                        _ => break,
                    }
                }
                (dir, rest)
            }
            // `self::x` → a child module of the current one.
            "self" => {
                let dir = self.rust_mod_dir(from);
                for k in (1..=rest.len()).rev() {
                    if let Some(file) = self.rust_child_module_file(from, &dir, &rest[..k]) {
                        return (Target::Internal(file), false);
                    }
                }
                (dir, rest)
            }
            name => {
                // 2018 paths: a bare first segment names a module declared in
                // this file (`mod editor; use editor::x;`) before it names an
                // extern crate — and an uppercase one is an item in scope
                // here (`use Kind::*;` for a local enum), never a crate.
                if name.starts_with(char::is_uppercase) {
                    return (Target::Internal(from.to_path_buf()), false);
                }
                let dir = self.rust_mod_dir(from);
                let local = match ctx.declares(from, name) {
                    Some(declared) => declared,
                    // No facts about `from`: fall back to the file existing.
                    None => self.rust_child_module_file(from, &dir, &[name]).is_some(),
                };
                if !local {
                    // Another crate of the project's own (a workspace's
                    // member, or the package's library from its binary or
                    // tests): its library's modules, as `crate::` reads
                    // them inside it. Else a crate from elsewhere.
                    let Some(lib) = self.rust_libs.get(name) else {
                        return (Target::External(name.to_string()), false);
                    };
                    if rest.is_empty() {
                        return (Target::Internal(lib.clone()), false);
                    }
                    return self
                        .rust_resolve_in(&self.rust_mod_dir(lib), rest, ctx, hops)
                        .unwrap_or_else(|| (Target::Internal(lib.clone()), true));
                }
                for k in (1..=segs.len()).rev() {
                    if let Some(file) = self.rust_child_module_file(from, &dir, &segs[..k]) {
                        return (Target::Internal(file), false);
                    }
                }
                (dir, &segs[..])
            }
        };
        self.rust_resolve_in(&base, rest, ctx, hops)
            .unwrap_or_else(|| (Target::Unresolved(path.to_string()), false))
    }

    /// Resolve module path `path` under module directory `base`: the longest
    /// prefix that is a module FILE wins (trailing segments are items); else
    /// a name the directory's owning module re-exports is followed to where it
    /// really lives; else the name is an item of the owning module itself.
    ///
    /// The flag says the answer is that last fallback. A name the owning
    /// module is known to define (the symbol index's items, consts and
    /// statics included) is not a fallback: it is found by
    /// [`Resolver::rust_reexport`].
    fn rust_resolve_in(
        &self,
        base: &Path,
        path: &[&str],
        ctx: &Consulted<'_>,
        hops: usize,
    ) -> Option<(Target, bool)> {
        for k in (1..=path.len()).rev() {
            if let Some(p) = self.rust_module_file(base, &path[..k]) {
                return Some((Target::Internal(p), false));
            }
        }
        let owner = self.rust_dir_owner(base)?;
        if hops < MAX_REEXPORT_HOPS
            && let Some(found) = self.rust_reexport(&owner, path, ctx, hops, false)
        {
            // A re-export followed to where the name lives — which is itself
            // an inference when the chain ended in this fallback further on.
            return Some(found);
        }
        // `crate::Item` / `super::Item`: an item of the owning module — the
        // fallback applies only now that no module (file or re-export) and no
        // known item named by the path exists.
        Some((Target::Internal(owner), true))
    }

    /// Follow `path`'s first segment `name` through the `use` declarations of
    /// module file `owner` — what `crate::name` / `super::name` means when no
    /// module file is called `name`:
    ///   1. an explicit `use a::b::name;` (a grouped `use` arrives expanded,
    ///      one path per name) says exactly where `name` lives:
    ///      `pub use editor::codeview;` makes `crate::codeview` mean
    ///      `editor/codeview.rs`, and `pub use clew_core::docs;` makes
    ///      `crate::docs` the external `clew_core`;
    ///   2. an item `owner` defines itself (per the symbol index, carried in
    ///      the [`RustCtx`]) is `owner`'s: it shadows whatever a glob brings
    ///      in;
    ///   3. a glob `use p::*;` supplies `name` only when `name` is known to be
    ///      in `p`: a module file under `p`, an item the symbol index lists in
    ///      `p`'s file, or a name `p` re-exports in turn. Never `p`'s own file
    ///      on a guess — that would claim every root item for the first glob.
    ///
    /// `reexports_only`: `owner` was reached THROUGH a glob, which exposes
    /// what `owner` re-exports, not its private imports.
    ///
    /// With the target comes whether it is inferred: a chain that ends in
    /// [`Resolver::rust_resolve_in`]'s fallback is a guess however many hops
    /// led there, and the edge must say so (a guess is not a dependency, and
    /// closes no cycle).
    fn rust_reexport(
        &self,
        owner: &Path,
        path: &[&str],
        ctx: &Consulted<'_>,
        hops: usize,
        reexports_only: bool,
    ) -> Option<(Target, bool)> {
        let (&name, rest) = path.split_first()?;
        let uses = || {
            ctx.uses_of(owner)
                .iter()
                .filter(move |u| u.reexport || !reexports_only)
        };
        for u in uses().filter(|u| !u.glob) {
            let useg = rust_path_segments(&u.path);
            if useg.len() > 1 && useg.last() == Some(&name) {
                // `use a::b::name;` — `name` is exactly this path.
                let mut full = useg.join("::");
                for s in rest {
                    full.push_str("::");
                    full.push_str(s);
                }
                match self.rust_path_inferred(&full, owner, ctx, hops + 1) {
                    found @ (Target::Internal(_) | Target::External(_), _) => return Some(found),
                    (Target::Unresolved(_), _) => continue,
                }
            }
        }
        if ctx.defines(owner, name) {
            return Some((Target::Internal(owner.to_path_buf()), false));
        }
        for u in uses().filter(|u| u.glob) {
            // The module the glob reads from; a guess itself when its path
            // ended in the fallback, and then so is all found through it.
            let (Target::Internal(pfile), guessed) =
                self.rust_path_inferred(&u.path, owner, ctx, hops + 1)
            else {
                continue;
            };
            if pfile == owner {
                continue;
            }
            let dir = self.rust_mod_dir(&pfile);
            if self.rust_module_file(&dir, &[name]).is_some() {
                return self
                    .rust_resolve_in(&dir, path, ctx, hops + 1)
                    .map(|(target, inferred)| (target, inferred || guessed));
            }
            if ctx.defines(&pfile, name) {
                return Some((Target::Internal(pfile), guessed));
            }
            if hops + 1 < MAX_REEXPORT_HOPS
                && let Some((target, inferred)) =
                    self.rust_reexport(&pfile, path, ctx, hops + 1, true)
            {
                return Some((target, inferred || guessed));
            }
        }
        None
    }

    /// What a resolved Rust `use` edge from `from` means (see [`EdgeKind`]):
    /// a re-export, a use of an enclosing module, a guess (`inferred`: the
    /// name was found nowhere), or a dependency. A guess is said to be one
    /// wherever it points — but a glob of an enclosing module names no item
    /// to guess about.
    fn rust_use_kind(
        &self,
        raw: &RawImport,
        from: &Path,
        target: &Target,
        inferred: bool,
    ) -> EdgeKind {
        let u = RustUse::parse(&raw.module);
        if u.reexport {
            return EdgeKind::Reexport;
        }
        let namespace = u.glob || u.names_an_enclosing_module();
        match target {
            Target::Internal(t) if self.is_rust_ancestor(t, from) => {
                if inferred && !namespace {
                    EdgeKind::Inferred
                } else {
                    EdgeKind::Enclosing
                }
            }
            _ if inferred => EdgeKind::Inferred,
            _ => EdgeKind::Use,
        }
    }

    /// Whether module file `module` encloses `file` in the module tree: `file`
    /// lives in `module`'s directory of submodules, at any depth.
    fn is_rust_ancestor(&self, module: &Path, file: &Path) -> bool {
        module != file && file.starts_with(self.rust_mod_dir(module))
    }

    /// The module FILE whose submodules live in `dir`: `dir/mod.rs`, the
    /// sibling `dir.rs`, or a crate root (`lib.rs` preferred over `main.rs`
    /// when a package has both: shared modules are normally the library's).
    fn rust_dir_owner(&self, dir: &Path) -> Option<PathBuf> {
        let mod_rs = dir.join("mod.rs");
        if self.has_file(&mod_rs) {
            return Some(mod_rs);
        }
        if let Some(name) = dir.file_name() {
            let mut sibling = name.to_os_string();
            sibling.push(".rs");
            let sibling = dir.with_file_name(sibling);
            if self.has_file(&sibling) {
                return Some(sibling);
            }
        }
        for root in ["lib.rs", "main.rs"] {
            let p = dir.join(root);
            if self.has_file(&p) {
                return Some(p);
            }
        }
        self.rust_crate_roots
            .iter()
            .find(|r| self.rust_mod_dir(r) == dir)
            .cloned()
    }

    /// Directory that `from`'s submodules live in.
    fn rust_mod_dir(&self, from: &Path) -> PathBuf {
        let stem = from.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parent = from.parent().unwrap_or(&self.root);
        if stem == "mod" || self.rust_crate_roots.iter().any(|r| r == from) {
            parent.to_path_buf()
        } else {
            parent.join(stem)
        }
    }

    /// A nonstandard Cargo target can be called `main.rs`/`lib.rs` outside
    /// auto-discovered locations. Without manifest contents its identity is
    /// unknown: prefer ordinary-module children, retaining the historical
    /// sibling lookup only when that produces no child and a sibling exists.
    fn rust_child_module_file(&self, from: &Path, dir: &Path, segs: &[&str]) -> Option<PathBuf> {
        self.rust_module_file(dir, segs).or_else(|| {
            let stem = from.file_stem().and_then(|s| s.to_str())?;
            if !matches!(stem, "lib" | "main") || self.rust_crate_roots.iter().any(|r| r == from) {
                return None;
            }
            self.rust_module_file(from.parent()?, segs)
        })
    }

    /// File for a module reached by `segs` under `base` (`segs` are dirs except
    /// the last, which is `<name>.rs` or `<name>/mod.rs`).
    fn rust_module_file(&self, base: &Path, segs: &[&str]) -> Option<PathBuf> {
        let (last, dirs) = segs.split_last()?;
        let mut dir = base.to_path_buf();
        for d in dirs {
            dir = dir.join(d);
        }
        let flat = dir.join(format!("{last}.rs"));
        if self.has_file(&flat) {
            return Some(flat);
        }
        let nested = dir.join(last).join("mod.rs");
        if self.has_file(&nested) {
            return Some(nested);
        }
        None
    }

    // --- Python ---

    fn resolve_python(&self, raw: &RawImport, from: &Path) -> Target {
        let level = raw.module.chars().take_while(|&c| c == '.').count();
        let dots = &raw.module[..level];
        // `from m import name` (recorded `m:name`): the package's submodule
        // `name` when it has one, else the module itself — whose
        // `__init__.py`, for a package, defines or imports the name, and runs
        // first in either case.
        let (body, name) = match raw.module[level..].split_once(PY_NAME_SEP) {
            Some((module, name)) => (module, Some(name)),
            None => (&raw.module[level..], None),
        };
        let segs: Vec<&str> = body.split('.').filter(|s| !s.is_empty()).collect();
        let named = |name| -> Vec<&str> {
            let mut segs = segs.clone();
            segs.push(name);
            segs
        };

        if level > 0 {
            // Relative: `.` = current package (dir), each extra dot goes up one.
            let mut base = from.parent().unwrap_or(&self.root).to_path_buf();
            for _ in 1..level {
                base = base.parent().unwrap_or(&self.root).to_path_buf();
            }
            // A package's own `__init__.py` does not import a name from
            // itself, so the package's reading never points back at `from`.
            return name
                .and_then(|name| self.python_module_file(&base, &named(name)))
                .or_else(|| {
                    self.python_module_file(&base, &segs)
                        .filter(|init| !segs.is_empty() || init != from)
                })
                .map(Target::Internal)
                .unwrap_or_else(|| match name {
                    Some(name) if segs.is_empty() => Target::Unresolved(format!("{dots}{name}")),
                    _ => Target::Unresolved(format!("{dots}{body}")),
                });
        }
        // Absolute: the bases Python itself searches, in its order. First
        // the directory the file runs from: its own folder for a script, or,
        // for a module of a package, the folder above its outermost package
        // (pytest's rootdir insertion, `python -m`). Then the source roots
        // the project's `pyproject.toml` names (setuptools' `where`,
        // maturin's `python-source`, pytest's `pythonpath`, …). Then the
        // project root and `src/`, as Pyright does: a `tests/` file's
        // `from cart import` of `src/cart.py` read as a package. Last, the
        // folders between that base and the root that are not packages
        // themselves (a service's folder in a monorepo).
        //
        // A folder inside a package is never a base: `import json` in
        // `flask/json/__init__.py` is the standard library's `json`, never
        // the file itself, nor `flask/typing.py` for `import typing`. And a
        // standard-library name is looked for only where Python would find
        // a file before its own: the running folder and the named roots.
        let package_dir = |d: &Path| {
            self.has_file(&d.join("__init__.py")) || self.has_file(&d.join("__init__.pyi"))
        };
        let mut running = from.parent().unwrap_or(&self.root);
        while running != self.root && package_dir(running) {
            match running.parent() {
                Some(up) => running = up,
                None => break,
            }
        }
        let mut bases: Vec<PathBuf> = vec![running.to_path_buf()];
        bases.extend(self.python_roots.iter().cloned());
        let stdlib = segs.first().is_some_and(|top| python_stdlib(top));
        if !stdlib {
            bases.push(self.root.clone());
            bases.push(self.root.join("src"));
            let mut p = running.parent();
            while let Some(d) = p {
                if !d.starts_with(&self.root) || d == self.root {
                    break;
                }
                if !package_dir(d) {
                    bases.push(d.to_path_buf());
                }
                p = d.parent();
            }
        }
        let mut seen: Vec<&Path> = Vec::new();
        for base in &bases {
            if seen.contains(&base.as_path()) {
                continue;
            }
            seen.push(base);
            if let Some(f) = name
                .and_then(|name| self.python_module_file(base, &named(name)))
                .or_else(|| self.python_module_file(base, &segs))
            {
                return Target::Internal(f);
            }
        }
        Target::External(segs.first().copied().unwrap_or(body).to_string())
    }

    /// The file module `segs` is under `base`: `a/b.py`, or the package
    /// `a/b/__init__.py`; else its stub, `a/b.pyi` or `a/b/__init__.pyi` —
    /// the source when there is one, as it is what runs, and the stub of a
    /// module that is compiled (a Rust or C extension's `.pyi`) or written
    /// apart from its code.
    fn python_module_file(&self, base: &Path, segs: &[&str]) -> Option<PathBuf> {
        let found = |ext: &str| -> Option<PathBuf> {
            if segs.is_empty() {
                let init = base.join(format!("__init__.{ext}"));
                return self.has_file(&init).then_some(init);
            }
            let (last, dirs) = segs.split_last()?;
            let mut dir = base.to_path_buf();
            for d in dirs {
                dir = dir.join(d);
            }
            let flat = dir.join(format!("{last}.{ext}"));
            if self.has_file(&flat) {
                return Some(flat);
            }
            let pkg = dir.join(last).join(format!("__init__.{ext}"));
            self.has_file(&pkg).then_some(pkg)
        };
        found("py").or_else(|| found("pyi"))
    }

    // --- JS / TS ---

    /// JS/TS specifiers — ES `import`/`export … from`, CommonJS `require()` and
    /// dynamic `import()` all carry the same kind of string: relative paths
    /// resolve against the importing file, tsconfig/jsconfig `paths` and
    /// `baseUrl` map aliases (`@/components/x`, `~/lib`), and anything else is
    /// a package.
    fn resolve_js(&self, raw: &RawImport, from: &Path) -> Target {
        let spec = raw.module.as_str();
        let is_relative =
            spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == "..";
        if is_relative {
            let base = from.parent().unwrap_or(&self.root);
            let joined = normalize(&base.join(spec));
            // `.`, `..` and a path ending in `/` name a directory — its
            // index — never a file beside it of the directory's name
            // (`src.ts` next to `src/`), as Node and TypeScript read them.
            let directory = matches!(spec, "." | "..")
                || spec.ends_with('/')
                || spec.ends_with("/.")
                || spec.ends_with("/..");
            let found = if directory {
                self.js_index(&joined)
            } else {
                self.js_file(&joined)
            };
            return found
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(spec.to_string()));
        }
        if spec.starts_with('/') {
            return Target::Unresolved(spec.to_string()); // absolute FS path, rare
        }
        // `node:fs`, `https://…`: never project files.
        if spec.contains(':') {
            return Target::External(spec.split(':').next().unwrap_or(spec).to_string());
        }
        if let Some(config) = self.ts_config_for(from) {
            if let Some(p) = self.resolve_ts_paths(config, spec) {
                return Target::Internal(p);
            }
            // `baseUrl` makes bare specifiers project-relative first.
            if let Some(base_url) = &config.base_url
                && let Some(p) = self.js_file(&normalize(&base_url.join(spec)))
            {
                return Target::Internal(p);
            }
        }
        // One of the project's own packages, by its name.
        if let Some(found) = self.resolve_js_package(spec) {
            return found;
        }
        // `@/x` and `~/x` can never be packages (a scope needs a name; npm
        // names have no `~`), so they are aliases; unconfigured, they mean
        // what the common templates point them at (Vite, Vue CLI, Next and
        // Nuxt `@/`, Nuxt and Remix `~/`): the project's `src/` (or Remix's
        // `app/`), else its root.
        if let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/")) {
            return ["src", "app", ""]
                .iter()
                .find_map(|dir| self.js_file(&normalize(&self.root.join(dir).join(rest))))
                .map(Target::Internal)
                .unwrap_or_else(|| Target::Unresolved(spec.to_string()));
        }
        Target::External(js_package(spec))
    }

    /// `spec` (`@acme/api`, `@acme/ui/button`) as one of the project's own
    /// packages exports it: the file its `exports` (or `main`, `module`,
    /// `types`) names for that subpath — a source file before a built one,
    /// and a built `dist/x.js` read as the `src/x.ts` it was built from —
    /// else, unexported, the file of that path in its `src/`, or in its
    /// folder. A package of the project's that has none is still the
    /// package: an external one, as before. `None` for a specifier that
    /// names none of the project's packages.
    fn resolve_js_package(&self, spec: &str) -> Option<Target> {
        let name = js_package(spec);
        let (dir, exports) = self.js_packages.get(&name)?;
        let rest = spec[name.len()..].trim_start_matches('/');
        let subpath = if rest.is_empty() {
            ".".to_string()
        } else {
            format!("./{rest}")
        };
        // The exported subpath: as written, or by a `*` pattern.
        let mut wanted: Vec<String> = Vec::new();
        for (pattern, files) in exports {
            if *pattern == subpath {
                wanted.extend(files.iter().cloned());
            } else if let Some((pre, post)) = pattern.split_once('*')
                && subpath.len() >= pre.len() + post.len()
                && subpath.starts_with(pre)
                && subpath.ends_with(post)
            {
                let captured = &subpath[pre.len()..subpath.len() - post.len()];
                wanted.extend(files.iter().map(|f| f.replacen('*', captured, 1)));
            }
        }
        let file = |f: &str| self.js_file(&normalize(&dir.join(f)));
        let found = wanted
            .iter()
            .find_map(|f| {
                file(f).or_else(|| {
                    // What a build made, read as what it was made from.
                    let f = f.trim_start_matches("./");
                    ["dist/", "build/", "out/", "lib/"]
                        .iter()
                        .find_map(|d| f.strip_prefix(d))
                        .and_then(|rest| {
                            let rest = rest.trim_end_matches(".d.ts");
                            file(&format!("src/{rest}"))
                        })
                })
            })
            .or_else(|| match rest {
                "" => ["src/index", "index", "src/main", "src/lib"]
                    .iter()
                    .find_map(|f| file(f)),
                rest => file(&format!("src/{rest}")).or_else(|| file(rest)),
            });
        Some(match found {
            Some(path) => Target::Internal(path),
            None => Target::External(name),
        })
    }

    /// The nearest tsconfig/jsconfig whose directory contains `from`.
    fn ts_config_for(&self, from: &Path) -> Option<&TsConfig> {
        // Sorted deepest first, so the first ancestor is the nearest.
        self.ts_configs.iter().find(|c| from.starts_with(&c.dir))
    }

    /// Apply `compilerOptions.paths`: the pattern with the longest prefix
    /// before its `*` wins (TypeScript's rule), and its targets are tried in
    /// order. `None` when no pattern matches or none of the winning pattern's
    /// targets exists — TypeScript then goes on to `baseUrl` and
    /// `node_modules`, so an alias whose local target is missing can still be
    /// an installed package of that name (`"config": ["./config"]` with no
    /// `./config` is the npm package `config`).
    fn resolve_ts_paths(&self, config: &TsConfig, spec: &str) -> Option<PathBuf> {
        let mut best: Option<(usize, &str, &Vec<String>)> = None;
        for (pattern, targets) in &config.paths {
            let captured = match pattern.split_once('*') {
                None if pattern == spec => "",
                None => continue,
                Some((pre, post)) => {
                    if spec.len() >= pre.len() + post.len()
                        && spec.starts_with(pre)
                        && spec.ends_with(post)
                    {
                        &spec[pre.len()..spec.len() - post.len()]
                    } else {
                        continue;
                    }
                }
            };
            // An exact pattern outranks every wildcard; among wildcards the
            // longest prefix wins.
            let rank = match pattern.split_once('*') {
                None => usize::MAX,
                Some((pre, _)) => pre.len(),
            };
            if best.is_none_or(|(r, _, _)| rank > r) {
                best = Some((rank, captured, targets));
            }
        }
        let (_, captured, targets) = best?;
        targets.iter().find_map(|target| {
            let substituted = target.replacen('*', captured, 1);
            self.js_file(&normalize(&config.paths_base.join(substituted)))
        })
    }

    /// The project file a module path names, trying what Node, TypeScript and
    /// the bundlers try: the path as written; TypeScript's ESM convention of
    /// importing the EMITTED name (`./util.js` for `util.ts`, `.jsx` for
    /// `.tsx`, `.mjs`/`.cjs` for `.mts`/`.cts`); an added extension; an index
    /// file.
    fn js_file(&self, joined: &Path) -> Option<PathBuf> {
        if self.has_file(joined) {
            return Some(joined.to_path_buf());
        }
        let emitted: &[(&str, &[&str])] = &[
            ("js", &["ts", "tsx", "d.ts"]),
            ("jsx", &["tsx"]),
            ("mjs", &["mts", "d.mts"]),
            ("cjs", &["cts", "d.cts"]),
        ];
        if let Some(ext) = joined.extension().and_then(|e| e.to_str())
            && let Some((_, sources)) = emitted.iter().find(|(e, _)| *e == ext)
        {
            let stem = joined.with_extension("");
            for src in *sources {
                let cand = with_added_ext(&stem, src);
                if self.has_file(&cand) {
                    return Some(cand);
                }
            }
        }
        for ext in JS_EXTS {
            let cand = with_added_ext(joined, ext);
            if self.has_file(&cand) {
                return Some(cand);
            }
        }
        self.js_index(joined)
    }

    /// The index file of directory `dir` (`index.ts`, `index.js`, …).
    fn js_index(&self, dir: &Path) -> Option<PathBuf> {
        JS_EXTS.iter().find_map(|ext| {
            let cand = dir.join(format!("index.{ext}"));
            self.has_file(&cand).then_some(cand)
        })
    }

    // --- Go ---

    fn resolve_go(&self, raw: &RawImport) -> Target {
        let path = raw.module.as_str();
        if let Some(prefix) = &self.go_module
            && let Some(rest) = path.strip_prefix(prefix.as_str())
            // Require a real path-segment boundary, so module `example.com/foo`
            // doesn't swallow the unrelated external `example.com/foobar`.
            && (rest.is_empty() || rest.starts_with('/'))
        {
            let rest = rest.trim_start_matches('/');
            let dir = if rest.is_empty() {
                self.root.clone()
            } else {
                self.root.join(rest)
            };
            // A Go package is a directory of files; point the edge at it.
            if self.dirs.contains(&dir) {
                return Target::Internal(dir);
            }
            return Target::Unresolved(path.to_string());
        }
        // Standard library / third-party.
        Target::External(path.to_string())
    }
}

/// First path segment of a JS bare specifier, keeping an `@scope/name` together.
fn js_package(spec: &str) -> String {
    if let Some(rest) = spec.strip_prefix('@') {
        let mut it = rest.splitn(3, '/');
        let scope = it.next().unwrap_or("");
        let name = it.next().unwrap_or("");
        return format!("@{scope}/{name}");
    }
    spec.split('/').next().unwrap_or(spec).to_string()
}

/// Lexically normalize a path, resolving `.` and `..` without touching disk.
/// The extensions a JS or TS import may leave off, in the order tried.
const JS_EXTS: &[&str] = &[
    "ts", "tsx", "d.ts", "mts", "cts", "js", "jsx", "mjs", "cjs", "json",
];

/// What a C or C++ `#include` can name: headers, and source files a build
/// includes whole (`x.inc`, `x.def`, a template's `.tpp`).
const C_INCLUDABLE: &[&str] = &[
    "h", "hh", "hpp", "hxx", "h++", "ipp", "tpp", "inl", "inc", "def", "c", "cc", "cpp", "cxx",
];

/// The C standard library's headers.
const C_STANDARD_HEADERS: &[&str] = &[
    "assert.h",
    "complex.h",
    "ctype.h",
    "errno.h",
    "fenv.h",
    "float.h",
    "inttypes.h",
    "iso646.h",
    "limits.h",
    "locale.h",
    "math.h",
    "setjmp.h",
    "signal.h",
    "stdalign.h",
    "stdarg.h",
    "stdatomic.h",
    "stdbit.h",
    "stdbool.h",
    "stdckdint.h",
    "stddef.h",
    "stdint.h",
    "stdio.h",
    "stdlib.h",
    "stdnoreturn.h",
    "string.h",
    "tgmath.h",
    "threads.h",
    "time.h",
    "uchar.h",
    "wchar.h",
    "wctype.h",
];

/// POSIX's own headers (beside its `sys/`, `arpa/`, `net/` and `netinet/`).
const POSIX_HEADERS: &[&str] = &[
    "unistd.h",
    "fcntl.h",
    "pthread.h",
    "dirent.h",
    "dlfcn.h",
    "poll.h",
    "termios.h",
    "netdb.h",
    "strings.h",
    "syslog.h",
    "sched.h",
    "semaphore.h",
    "spawn.h",
    "glob.h",
    "grp.h",
    "pwd.h",
    "regex.h",
    "libgen.h",
    "fnmatch.h",
    "utime.h",
    "aio.h",
    "mqueue.h",
    "ftw.h",
];

/// The library a header that is no project file belongs to (see
/// [`Resolver::resolve_c`]).
fn c_library(path: &str) -> String {
    let first = path.split('/').next().unwrap_or(path);
    if (!path.contains('/') && !path.contains('.')) || first == "bits" {
        // `<vector>`, `<cstdio>`; GCC's own `<bits/…>`.
        return "std".into();
    }
    if C_STANDARD_HEADERS.contains(&path) {
        return "libc".into();
    }
    if POSIX_HEADERS.contains(&path) || matches!(first, "sys" | "arpa" | "net" | "netinet") {
        return "posix".into();
    }
    match path.contains('/') {
        true => first.to_string(),
        false => path
            .rsplit_once('.')
            .map_or(path, |(stem, _)| stem)
            .to_string(),
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Append `.ext` to a path's file name (so `foo` → `foo.ts`, keeping any dots).
fn with_added_ext(p: &Path, ext: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".");
    name.push(ext);
    p.with_file_name(name)
}

/// `base` joined with each of `parts` in turn (`a.b.C`'s parts: `a/b/C`).
/// An Elixir alias as Mix names its file: `OrderItem` is `order_item`,
/// `HTTPClient` `http_client` (Macro.underscore's rule).
fn snake_case(part: &str) -> String {
    let chars: Vec<char> = part.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let after_lower =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let before_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if i > 0 && (after_lower || (before_lower && chars[i - 1].is_uppercase())) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn join_all(base: &Path, parts: &[&str]) -> PathBuf {
    parts
        .iter()
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

/// Of `files`, the one sharing the most of its path with `from` — the same
/// application's, the same package's — the first one on a tie.
fn nearest<'a>(files: impl Iterator<Item = &'a PathBuf>, from: &Path) -> Option<&'a PathBuf> {
    let shared = |f: &Path| {
        f.components()
            .zip(from.components())
            .take_while(|(a, b)| a == b)
            .count()
    };
    files
        .fold(None, |best: Option<(&PathBuf, usize)>, f| {
            let depth = shared(f);
            match best {
                Some((_, held)) if held >= depth => best,
                _ => Some((f, depth)),
            }
        })
        .map(|(f, _)| f)
}

/// The Haskell source roots that exist (are among `dirs`): for each package
/// directory `declared` names — with the source directories its description
/// gives — those directories, then its conventional `src`, `app`, `lib` and
/// `test` and itself; then the same conventional ones of the project root.
fn haskell_roots(
    root: &Path,
    declared: &[(PathBuf, Vec<String>)],
    dirs: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    const CONVENTIONAL: &[&str] = &["src", "app", "lib", "test"];
    let mut roots: Vec<PathBuf> = Vec::new();
    let packages = declared
        .iter()
        .map(|(dir, named)| (dir.as_path(), named.as_slice()))
        .chain([(root, &[][..])]);
    for (package, named) in packages {
        let candidates = named
            .iter()
            .map(|d| normalize(&package.join(d)))
            .chain(CONVENTIONAL.iter().map(|d| package.join(d)))
            .chain([package.to_path_buf()]);
        for candidate in candidates {
            if dirs.contains(&candidate) && !roots.contains(&candidate) {
                roots.push(candidate);
            }
        }
    }
    roots
}

// ---------------------------------------------------------------------------
// tsconfig.json / jsconfig.json
// ---------------------------------------------------------------------------

/// Byte cap for one tsconfig/jsconfig: the files ship with the repository, so
/// their size and type are attacker-chosen (same reasoning as `go.mod`).
const MAX_TS_CONFIG_BYTES: u64 = 1024 * 1024;
/// At most this many configs are read per project (monorepos have one per
/// package; a pathological tree could have thousands). The shallowest are
/// kept (see [`ts_config_paths`]).
const MAX_TS_CONFIGS: usize = 64;
/// At most this many entries of one `extends` array are followed, in file
/// order; the rest are ignored as though the config did not name them. Real
/// configs name one to three bases, but an array is attacker-chosen, and
/// each entry used to be followed, and listed among the sources, however
/// many a 1 MiB file could hold.
const MAX_TS_EXTENDS_ENTRIES: usize = 32;
/// At most this many files — the configs, then every base their `extends`
/// chains name, in the order they are reached — are read and listed per
/// resolve ([`TsConfigs::reads`]). A base reached once the budget is spent
/// is ignored like one past [`MAX_TS_EXTENDS_ENTRIES`]: not read, not merged,
/// not listed. Real projects reach a few dozen; bases that fan out through
/// every level of a chain could otherwise reach a million.
const MAX_TS_SOURCES: usize = 1024;

/// Read every `tsconfig.json` / `jsconfig.json` in the project's file list,
/// following relative `extends`. Local projects: the reads go through the
/// bounded, plain-file-only state reader ([`read_ts_config_text`]). Deepest
/// directories first. A config or base that is there and is not read — too
/// large, no plain text file, not readable — is named in the note on what
/// was left out ([`TsConfigs::cap_note`]), as a remote project's is: taken
/// for one not there, its aliases resolved as packages without a word.
fn read_ts_configs(root: &Path, files: &[PathBuf]) -> TsConfigs {
    let not_read = std::cell::RefCell::new(Vec::new());
    let read = |path: &Path| match read_ts_config_text(path) {
        Ok(text) => text,
        Err(why) => {
            // Every file a resolve reads is under the root (`TsLoader`).
            if let Some(rel) = path
                .strip_prefix(root)
                .ok()
                .filter(|r| r.to_str().is_some())
            {
                not_read
                    .borrow_mut()
                    .push((clew_core::rel_string(rel), why));
            }
            None
        }
    };
    let mut configs = ts_configs_from(root, files, &read);
    let mut not_read = not_read.into_inner();
    not_read.sort_by(|a, b| a.0.cmp(&b.0));
    configs.left_out.files = not_read;
    configs
}

/// One local config file's text — `Ok(None)` when it is not there — through
/// the bounded, plain-file-only state reader, remembered per path while its
/// size and modification time stay the same. A resolver is built anew — by
/// the import job, off the UI thread — for every re-resolve (a file created,
/// deleted or renamed, a metadata file edited), and re-reading every config
/// each time cost up to 64 opens and reads per build for files that almost
/// never change; an unchanged file now costs one `lstat`. A path that is no
/// longer a plain file is refused, and a changed one goes back through the
/// checked reader, so the cache never admits what a fresh read would refuse.
fn read_ts_config_text(path: &Path) -> Result<Option<String>, TsNotRead> {
    type Stamp = (u64, Option<std::time::SystemTime>);
    type Read = Result<Option<String>, TsNotRead>;
    type Texts = HashMap<PathBuf, (Stamp, Read)>;
    static CACHE: std::sync::LazyLock<std::sync::Mutex<Texts>> =
        std::sync::LazyLock::new(Default::default);
    /// Several projects' worth of configs; beyond that the cache starts over.
    const MAX_CACHED: usize = 1024;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(TsNotRead::Unreadable(e.to_string())),
    };
    if !meta.is_file() {
        return Err(TsNotRead::Refused(clew_protocol::Refusal::NotPlainFile));
    }
    let stamp: Stamp = (meta.len(), meta.modified().ok());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached, read)) = cache.get(path)
        && *cached == stamp
    {
        return read.clone();
    }
    let read = clew_core::statefile::read_capped_checked(path, MAX_TS_CONFIG_BYTES)
        .map_err(|e| TsNotRead::of_read(&e));
    // A read that failed is not remembered: the permission put right, or
    // the moment of `EIO` past, leaves the size and time as they were.
    if !matches!(read, Err(TsNotRead::Unreadable(_))) {
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), (stamp, read.clone()));
    }
    read
}

/// The project's `tsconfig.json` / `jsconfig.json` files: one per directory —
/// the `tsconfig.json` where a directory has both, as TypeScript ignores the
/// jsconfig there — shallowest first, at most [`MAX_TS_CONFIGS`] of them.
/// Shallowest first is what makes the cap safe: in path order a monorepo's
/// `packages/…` configs used up the budget before the root config, the one
/// every file outside a package falls back to.
fn ts_config_paths(files: &[PathBuf]) -> Vec<&PathBuf> {
    let mut paths = ts_config_candidates(files);
    paths.truncate(MAX_TS_CONFIGS);
    paths
}

/// Every directory's config, in the order [`ts_config_paths`] keeps them,
/// before the cap.
fn ts_config_candidates(files: &[PathBuf]) -> Vec<&PathBuf> {
    let mut found: Vec<(usize, &Path, bool, &PathBuf)> = files
        .iter()
        .filter_map(|f| {
            let is_jsconfig = match f.file_name()?.to_str()? {
                "tsconfig.json" => false,
                "jsconfig.json" => true,
                _ => return None,
            };
            let dir = f.parent()?;
            Some((dir.components().count(), dir, is_jsconfig, f))
        })
        .collect();
    found.sort();
    // Sorted, a directory's tsconfig comes right before its jsconfig.
    found.dedup_by(|later, kept| later.1 == kept.1);
    found.into_iter().map(|(.., f)| f).collect()
}

/// Whether `path` is a file a local resolver reads for the WHOLE project —
/// go.mod, pubspec.yaml, a composer.json (PHP's PSR-4 map), a Haskell
/// package description (`.cabal`, `package.yaml`), a Cargo.toml (a crate's
/// library name and targets), a pyproject.toml (Python's source roots), a
/// build.zig (the modules it names), a
/// tsconfig/jsconfig or a base one extends by the usual naming
/// (`tsconfig.base.json`) — so a change to it re-resolves every file against
/// a resolver that reads it again.
pub fn is_resolution_metadata(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        name,
        "go.mod"
            | "pubspec.yaml"
            | "composer.json"
            | "Cargo.toml"
            | "pyproject.toml"
            | "package.json"
            | "build.zig"
    ) || clew_core::imports::is_haskell_manifest(name)
        || ((name.starts_with("tsconfig") || name.starts_with("jsconfig"))
            && name.ends_with(".json"))
}

/// Parse the project's configs with `read` supplying each file's text
/// (`None` when missing or refused). Deepest directories first. A base
/// several configs extend is read and parsed once for them all, and listed
/// once among their sources.
fn ts_configs_from(
    root: &Path,
    files: &[PathBuf],
    read: &impl Fn(&Path) -> Option<String>,
) -> TsConfigs {
    let mut paths = ts_config_candidates(files);
    let past_cap = paths.len().saturating_sub(MAX_TS_CONFIGS);
    paths.truncate(MAX_TS_CONFIGS);
    let mut loader = TsLoader::new(root, read);
    // The configs themselves are sources before any base a chain names
    // (there are at most `MAX_TS_CONFIGS` of them, well within the budget):
    // an edit to one changes it, whatever the others extend.
    for path in &paths {
        loader.admit(path);
    }
    let mut configs: Vec<TsConfig> = paths
        .into_iter()
        .filter_map(|path| loader.config(path, 0))
        .collect();
    configs.sort_by_key(|c| std::cmp::Reverse(c.dir.components().count()));
    TsConfigs {
        configs,
        sources: loader.sources,
        left_out: TsLeftOut {
            configs: past_cap,
            extends: loader.extends_ignored,
            bases: loader.refused.len(),
            files: Vec::new(),
        },
    }
}

/// Whether the project has any `tsconfig.json` / `jsconfig.json` at all.
pub fn has_ts_configs(files: &[PathBuf]) -> bool {
    !ts_config_paths(files).is_empty()
}

/// The TypeScript/JavaScript path-mapping configs of a REMOTE project, whose
/// files are on another host: their text is fetched through `fetch`, a batch
/// of project-relative paths at a time, with what the host said of each (the
/// client passes the server's `ReadSources`, which confines and caps every
/// read there, asked until each path is settled: [`crate::sources::read`]).
/// The configs are fetched first; the relative `extends` they name (a
/// `tsconfig.base.json`) are fetched in later rounds, as deep as a chain is
/// followed ([`MAX_TS_EXTENDS_DEPTH`]). Nothing outside the project is ever
/// asked for, and nothing twice.
///
/// A file the host says is not there is gone. One too large to read, no
/// plain text file, or one the host could not read — this user may not — is
/// not read, and said to be, with why ([`TsConfigs::cap_note`]): asked
/// again, the host would say the same, and the other configs apply. One
/// such file failed the fetch, and kept every alias of the project from
/// applying for as long as it stayed so. One the host did not answer for is
/// not known: taken for one not there, it resolved the configs as though it
/// were deleted, and its aliases as packages. The fetch fails instead
/// (`Err`, with why), and the caller keeps the aliases it had and asks
/// again, or says alias resolution is unavailable.
pub async fn load_remote_ts_configs<F, Fut>(
    root: &Path,
    files: &[PathBuf],
    mut fetch: F,
) -> Result<TsConfigs, String>
where
    F: FnMut(Vec<String>) -> Fut,
    Fut: std::future::Future<Output = crate::sources::HostSources>,
{
    let rel = |p: &Path| {
        p.strip_prefix(root)
            .ok()
            .filter(|r| r.to_str().is_some())
            .map(clew_core::rel_string)
            .filter(|r| clew_core::statefile::safe_rel(r))
    };
    let mut texts: HashMap<PathBuf, String> = HashMap::new();
    let mut not_read: Vec<(String, TsNotRead)> = Vec::new();
    let mut asked: HashSet<PathBuf> = HashSet::new();
    let mut want: Vec<PathBuf> = ts_config_paths(files).into_iter().cloned().collect();
    // The configs, then one round per level of `extends` a chain is
    // followed to.
    for _ in 0..=MAX_TS_EXTENDS_DEPTH {
        let batch: Vec<PathBuf> = want.drain(..).filter(|p| asked.insert(p.clone())).collect();
        let rels: Vec<String> = batch.iter().filter_map(|p| rel(p)).collect();
        if rels.is_empty() {
            break;
        }
        let got = fetch(rels).await;
        if let Some((unread, why)) = got.unread.into_iter().next() {
            return Err(why.unwrap_or_else(|| tr!("{file} could not be read there", file = unread)));
        }
        for (r, text) in got.files {
            match text.len() as u64 {
                size if size > MAX_TS_CONFIG_BYTES => {
                    not_read.push((r, TsNotRead::TooLarge(size)));
                }
                _ => {
                    texts.insert(root.join(r), text);
                }
            }
        }
        let too_large = got.too_large.into_iter();
        not_read.extend(too_large.map(|(r, size)| (r, TsNotRead::TooLarge(size))));
        let refused = got.refused.into_iter();
        not_read.extend(refused.map(|(r, why)| (r, TsNotRead::Refused(why))));
        let unreadable = got.unreadable.into_iter();
        not_read.extend(unreadable.map(|(r, why)| (r, TsNotRead::Unreadable(why))));
        // Parse what arrived, noting every base it names that has not been
        // asked for yet: the next round's batch.
        let unasked = std::cell::RefCell::new(Vec::new());
        let read = |p: &Path| match texts.get(p) {
            Some(text) => Some(text.clone()),
            None => {
                if !asked.contains(p) {
                    unasked.borrow_mut().push(p.to_path_buf());
                }
                None
            }
        };
        let _ = ts_configs_from(root, files, &read);
        want = unasked.into_inner();
    }
    let read = |p: &Path| texts.get(p).cloned();
    let mut configs = ts_configs_from(root, files, &read);
    not_read.sort_by(|a, b| a.0.cmp(&b.0));
    configs.left_out.files = not_read;
    Ok(configs)
}

/// One config loaded on its own ([`TsLoader::config`]), for the tests that
/// parse a single config.
#[cfg(test)]
pub(crate) fn load_ts_config(
    root: &Path,
    path: &Path,
    read: &impl Fn(&Path) -> Option<String>,
    depth: usize,
) -> Option<TsConfig> {
    TsLoader::new(root, read).config(path, depth)
}

/// How far an `extends` chain is followed: a base this many levels below
/// the config still contributes its own settings, but the bases it names are
/// not read. A bound for hostile chains; real ones are a level or two deep.
const MAX_TS_EXTENDS_DEPTH: usize = 4;

/// Loads configs and the chains they extend for one resolve (every config a
/// resolver is built with: [`ts_configs_from`]). Each file is read and parsed
/// at most once, and merged with its chain at most once per depth it is
/// reached at, however many configs extend it, however often one `extends`
/// array names it and through however many diamonds. Every entry used to be
/// followed afresh, so a config whose `extends` names itself a hundred times
/// cost up to 100⁴ parses, and recorded as many paths among its sources.
///
/// What it reads is bounded too: at most [`MAX_TS_EXTENDS_ENTRIES`] bases per
/// `extends` array, and [`MAX_TS_SOURCES`] files in all.
struct TsLoader<'a, R> {
    root: &'a Path,
    read: &'a R,
    /// Each file's own settings: `None` when it could not be read or parsed.
    files: HashMap<PathBuf, Option<Rc<TsFile>>>,
    /// Each file merged with the chain it extends, by the depth it was
    /// reached at, which decides how much of the chain is followed.
    layers: HashMap<(PathBuf, usize), Option<Rc<TsLayer>>>,
    /// Every file admitted to this resolve ([`TsLoader::admit`]): what its
    /// configs are built from ([`TsConfigs::reads`]), and the only files it
    /// reads.
    sources: HashSet<PathBuf>,
    /// The bases refused once the budget was spent: not read.
    refused: HashSet<PathBuf>,
    /// The `extends` entries ignored past [`MAX_TS_EXTENDS_ENTRIES`], over
    /// every file parsed.
    extends_ignored: usize,
}

/// One config file's own settings, before the chain it extends is merged in.
struct TsFile {
    /// The bases its `extends` names that are followed, in file order: the
    /// relative ones (`./`, `../`) among its first [`MAX_TS_EXTENDS_ENTRIES`],
    /// normalized, `.json` added where missing, and only those inside the
    /// project.
    bases: Vec<PathBuf>,
    /// The entries of its `extends` array past [`MAX_TS_EXTENDS_ENTRIES`].
    extends_ignored: usize,
    /// `compilerOptions.baseUrl`, absolute.
    base_url: Option<PathBuf>,
    /// `compilerOptions.paths` in file order, with this file's directory.
    paths: Option<Rc<(TsPaths, PathBuf)>>,
    /// The project configs its `references` name (`./tsconfig.app.json`),
    /// inside the project: a "solution" config — Vite's — sets no paths of
    /// its own, and its app's config does.
    references: Vec<PathBuf>,
}

/// A config merged with the chain it extends, its `paths` not yet anchored:
/// what they resolve against depends on the final `baseUrl`, which a config
/// further down the chain may still set.
#[derive(Default)]
struct TsLayer {
    /// `compilerOptions.baseUrl`, absolute: resolved against the config that
    /// set it.
    base_url: Option<PathBuf>,
    /// `compilerOptions.paths` in file order, with the directory of the
    /// config that declared them (shared down the chain, not copied).
    paths: Option<Rc<(TsPaths, PathBuf)>>,
}

impl<'a, R: Fn(&Path) -> Option<String>> TsLoader<'a, R> {
    fn new(root: &'a Path, read: &'a R) -> Self {
        TsLoader {
            root,
            read,
            files: HashMap::new(),
            layers: HashMap::new(),
            sources: HashSet::new(),
            refused: HashSet::new(),
            extends_ignored: 0,
        }
    }

    /// Admit `path` among this resolve's sources, if it is one already or
    /// the budget ([`MAX_TS_SOURCES`]) has room: only an admitted file is
    /// read, merged and listed. Once the budget is spent a new base is
    /// refused, wherever and however often a chain names it, so every
    /// config that reaches it sees the same answer.
    fn admit(&mut self, path: &Path) -> bool {
        if self.sources.contains(path) {
            return true;
        }
        if self.sources.len() >= MAX_TS_SOURCES {
            self.refused.insert(path.to_path_buf());
            return false;
        }
        self.sources.insert(path.to_path_buf());
        true
    }

    /// The config at `path`, reached `depth` levels down an `extends` chain
    /// (0 for a config of the project's own): parsed (JSONC: comments and
    /// trailing commas allowed) and merged with what it `extends` (relative
    /// paths inside the project only; a package base like
    /// `@tsconfig/node20` is not on this disk). `read` returns a file's text,
    /// `None` when missing or refused.
    ///
    /// Merged the way TypeScript merges: a setting of the extending config
    /// overrides the base's (and a later entry of an `extends` array an
    /// earlier one); `baseUrl` is relative to the config that sets it; and
    /// `paths` resolve against the FINAL `baseUrl` — wherever in the chain it
    /// was set — else against the directory of the config that declared
    /// `paths`.
    fn config(&mut self, path: &Path, depth: usize) -> Option<TsConfig> {
        let dir = path.parent()?.to_path_buf();
        let layer = self.layer(path, depth)?;
        // A solution config, of no aliases of its own: those of the first
        // config it references that has some — the app's, whose files are
        // the ones that use them.
        if layer.paths.is_none() && layer.base_url.is_none() && depth < MAX_TS_EXTENDS_DEPTH {
            let references = self
                .file(path)
                .map(|f| f.references.clone())
                .unwrap_or_default();
            for reference in references {
                if !self.admit(&reference) {
                    continue;
                }
                if let Some(found) = self.config(&reference, depth + 1)
                    && (!found.paths.is_empty() || found.base_url.is_some())
                {
                    return Some(TsConfig { dir, ..found });
                }
            }
        }
        let (paths, declared_in) = match &layer.paths {
            Some(paths) => paths.as_ref().clone(),
            None => (Vec::new(), dir.clone()),
        };
        Some(TsConfig {
            paths_base: layer.base_url.clone().unwrap_or(declared_in),
            base_url: layer.base_url.clone(),
            paths,
            dir,
        })
    }

    /// `path`'s own settings, read and parsed the first time they are needed.
    fn file(&mut self, path: &Path) -> Option<Rc<TsFile>> {
        if let Some(file) = self.files.get(path) {
            return file.clone();
        }
        let file = self.parse(path).map(Rc::new);
        self.extends_ignored += file.as_ref().map_or(0, |f| f.extends_ignored);
        self.files.insert(path.to_path_buf(), file.clone());
        file
    }

    fn parse(&self, path: &Path) -> Option<TsFile> {
        let text = (self.read)(path)?;
        let json: serde_json::Value = serde_json::from_str(&strip_jsonc(&text)).ok()?;
        let dir = path.parent()?;
        let mut extends_ignored = 0;
        let named: Vec<&str> = match json.get("extends") {
            Some(serde_json::Value::String(s)) => vec![s.as_str()],
            Some(serde_json::Value::Array(a)) => {
                extends_ignored = a.len().saturating_sub(MAX_TS_EXTENDS_ENTRIES);
                a.iter()
                    .take(MAX_TS_EXTENDS_ENTRIES)
                    .filter_map(|v| v.as_str())
                    .collect()
            }
            _ => Vec::new(),
        };
        let bases = named
            .into_iter()
            .filter(|base| base.starts_with("./") || base.starts_with("../"))
            .filter_map(|base| {
                let mut p = normalize(&dir.join(base));
                if p.extension().is_none_or(|e| e != "json") {
                    p = with_added_ext(&p, "json");
                }
                // Never read outside the project.
                p.starts_with(self.root).then_some(p)
            })
            .collect();
        let opts = json.get("compilerOptions");
        let base_url = opts
            .and_then(|o| o.get("baseUrl"))
            .and_then(|b| b.as_str())
            .map(|b| normalize(&dir.join(b)));
        let paths = opts
            .and_then(|o| o.get("paths"))
            .and_then(|p| p.as_object())
            .map(|paths| {
                let paths = paths
                    .iter()
                    .map(|(pattern, targets)| {
                        let targets = match targets {
                            serde_json::Value::Array(a) => a
                                .iter()
                                .filter_map(|t| t.as_str().map(str::to_string))
                                .collect(),
                            serde_json::Value::String(s) => vec![s.clone()],
                            _ => Vec::new(),
                        };
                        (pattern.clone(), targets)
                    })
                    .collect();
                Rc::new((paths, dir.to_path_buf()))
            });
        let references = json
            .get("references")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("path")?.as_str())
            .filter(|r| r.starts_with("./") || r.starts_with("../"))
            .take(MAX_TS_EXTENDS_ENTRIES)
            .filter_map(|r| {
                let mut p = normalize(&dir.join(r));
                if p.extension().is_none_or(|e| e != "json") {
                    p = p.join("tsconfig.json");
                }
                p.starts_with(self.root).then_some(p)
            })
            .collect();
        Some(TsFile {
            bases,
            extends_ignored,
            base_url,
            paths,
            references,
        })
    }

    /// `path` merged with the chain it extends, as reached at `depth`.
    fn layer(&mut self, path: &Path, depth: usize) -> Option<Rc<TsLayer>> {
        let key = (path.to_path_buf(), depth);
        if let Some(layer) = self.layers.get(&key) {
            return layer.clone();
        }
        let layer = self
            .file(path)
            .map(|file| Rc::new(self.merge(&file, depth)));
        self.layers.insert(key, layer.clone());
        layer
    }

    /// `file`'s bases first, in order, each overriding the one before; then
    /// its own settings over them all. A base is followed only if it is
    /// admitted among the sources ([`TsLoader::admit`]), and listed then
    /// whether or not it can be read: creating it changes the config too.
    fn merge(&mut self, file: &TsFile, depth: usize) -> TsLayer {
        let mut out = TsLayer::default();
        if depth < MAX_TS_EXTENDS_DEPTH {
            for base in &file.bases {
                if !self.admit(base) {
                    continue;
                }
                if let Some(b) = self.layer(base, depth + 1) {
                    if b.base_url.is_some() {
                        out.base_url.clone_from(&b.base_url);
                    }
                    if b.paths.is_some() {
                        out.paths.clone_from(&b.paths);
                    }
                }
            }
        }
        if file.base_url.is_some() {
            out.base_url.clone_from(&file.base_url);
        }
        if file.paths.is_some() {
            out.paths.clone_from(&file.paths);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Rust crate roots
// ---------------------------------------------------------------------------

// (Inline-module scoping of `use` paths — `scope_rust_imports` — lives in
// `clew_core::rustscope`, shared with the server's project snapshot.)

/// Whether `file` is the root module of some Rust crate target, per Cargo's
/// auto-discovery. Each such file starts its own `crate::` namespace:
///   `src/lib.rs`, `src/main.rs`                     — lib / default binary
///   `src/bin/<n>.rs`, `src/bin/<n>/main.rs`         — extra binaries
///   `examples|tests|benches/<n>.rs`, `…/<n>/main.rs`
///
/// The target directories only count where Cargo would look for them —
/// `bin` under a package's `src/`, the others directly at a package root
/// (identified by its `Cargo.toml` in `files`). An ordinary module directory
/// that happens to be called `tests` (e.g. `src/tests/helper.rs`) is NOT a
/// target and must keep resolving against its enclosing crate.
///
/// Manifest-declared paths (`[[bin]] path = …`, `[lib] path = …`) are not
/// read: they are rare, and a wrong guess would mis-resolve a whole crate.
/// Such a target's files simply keep resolving against the enclosing crate,
/// which is the pre-existing behaviour.
fn is_rust_crate_root(file: &Path, files: &HashSet<PathBuf>) -> bool {
    if file.extension().is_none_or(|e| e != "rs") {
        return false;
    }
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let Some(parent) = file.parent() else {
        return false;
    };
    let parent_name = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let package_root_at =
        |dir: Option<&Path>| dir.is_some_and(|pkg| files.contains(&pkg.join("Cargo.toml")));
    // `main.rs` / `lib.rs` are target roots only in the places Cargo puts
    // them: `<pkg>/src/`, `src/bin/<n>/main.rs`, and
    // `{examples,tests,benches}/<n>/main.rs` at a package root. An ordinary
    // nested module that happens to be named `main.rs` (e.g.
    // `src/commands/main.rs`) is NOT a new `crate::` namespace — treating
    // it as one detached its whole subtree from the enclosing crate.
    if matches!(name, "lib.rs" | "main.rs") {
        if parent_name == "src" {
            return true;
        }
        let grand = parent.parent();
        let grand_name = grand
            .and_then(|g| g.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let nested_main = match grand_name {
            // `<pkg>/src/bin/<n>/main.rs`.
            "bin" => {
                name == "main.rs"
                    && grand
                        .and_then(|g| g.parent())
                        .and_then(|s| s.file_name())
                        .is_some_and(|n| n == "src")
            }
            // `<pkg>/{examples,tests,benches}/<n>/main.rs`.
            "examples" | "tests" | "benches" => {
                name == "main.rs" && package_root_at(grand.and_then(|g| g.parent()))
            }
            _ => false,
        };
        if nested_main {
            return true;
        }
        // Otherwise it is `<n>.rs` with `<n>` = `main` / `lib`, which the
        // flat target directories below take like any other name
        // (`src/bin/main.rs`, `tests/main.rs`).
    }
    match parent_name {
        "bin" => {
            // `<pkg>/src/bin/<n>.rs`: bin must sit under the package's src.
            let src = parent.parent();
            src.and_then(|s| s.file_name()).is_some_and(|n| n == "src")
                && package_root_at(src.and_then(|s| s.parent()))
        }
        "examples" | "tests" | "benches" => package_root_at(parent.parent()),
        _ => false,
    }
}

// go.mod / pubspec.yaml metadata readers live in clew-core, shared with the
// server's project snapshot (a remote resolver gets these over the wire).
use clew_core::imports::{read_dart_package, read_go_module};

// ---------------------------------------------------------------------------
// The graph
// ---------------------------------------------------------------------------

/// Whole-project import graph. Out-edges (resolved from each file's raw imports)
/// are the source of truth; the reverse index over internal edges is derived.
///
/// Changed only through [`ImportGraph::apply`], a batch at a time, which the
/// app runs off the UI thread on a working copy of the graph the window shows
/// ([`ImportGraph::applied_to`]). Every per-file value is shared (`Arc`), so
/// that copy costs a key per file, not a deep copy of every edge.
#[derive(Debug, Default, Clone)]
pub struct ImportGraph {
    /// Cached extraction per file — kept so a structural change can re-resolve
    /// without re-reading files.
    raw: HashMap<PathBuf, Arc<Vec<RawImport>>>,
    out: HashMap<PathBuf, Arc<Vec<Edge>>>,
    /// Internal target file → files that import it.
    rev: HashMap<PathBuf, Arc<Vec<PathBuf>>>,
    /// The Rust facts other files resolve through, kept in step with `raw`
    /// (plus the items the symbol index lists per file).
    ctx: RustCtx,
    /// Per Rust file, the files whose facts its resolution read (see
    /// [`Consulted`]); only files that read any.
    reads: HashMap<PathBuf, Arc<Vec<PathBuf>>>,
    /// The reverse of `reads`: a file → the files whose resolution read its
    /// facts, which are exactly the ones to resolve again when they change.
    readers: HashMap<PathBuf, Arc<HashSet<PathBuf>>>,
    /// The Go and Java package directories of the resolver last applied
    /// with ([`Resolver::packages`]).
    packages: Arc<HashMap<PathBuf, Vec<PathBuf>>>,
    /// The C# namespaces the files declare, kept in step with `raw`.
    namespaces: Namespaces,
}

/// One file as the import graph takes it: the specifiers its import
/// statements write down and, for Rust, the items it defines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileImports {
    pub raw: Vec<RawImport>,
    /// See [`rust_item_keys`]; empty for other languages.
    pub items: RustItems,
}

/// What an [`ImportBatch`] does to one file.
#[derive(Debug, Clone, PartialEq)]
enum FileChange {
    /// The file's current imports (and items).
    Set(Queued),
    /// As `Set`, unless the resolver's file set no longer lists the file,
    /// which makes it a deletion: how a remote publication reports a file it
    /// has nothing for (see [`ImportBatch::set_if_listed`]).
    SetIfListed(Queued),
    /// The file is gone: its node and its out-edges leave the graph.
    Removed,
}

/// One file's [`FileImports`] as a batch holds them: the specifiers behind an
/// `Arc` the graph that installs them shares. The window keeps a batch until
/// its job lands (a job that fails runs it again), and the job reads it
/// there: installing a file costs a pointer, not a copy of its imports.
#[derive(Debug, Clone, PartialEq)]
struct Queued {
    raw: Arc<Vec<RawImport>>,
    items: RustItems,
}

impl From<FileImports> for Queued {
    fn from(imports: FileImports) -> Self {
        Queued {
            raw: Arc::new(imports.raw),
            items: imports.items,
        }
    }
}

/// Changes for [`ImportGraph::apply`], merged per file: the later change to a
/// file replaces the earlier one, so a burst of edits costs one resolution.
#[derive(Debug, Clone, Default)]
pub struct ImportBatch {
    files: HashMap<PathBuf, FileChange>,
    /// Every file resolves again against a new resolver.
    reresolve: bool,
    /// Start from an empty graph: the batch restates the whole project.
    reset: bool,
}

impl ImportBatch {
    /// `file`'s current imports, replacing any change queued for it before.
    pub fn set(&mut self, file: PathBuf, imports: FileImports) {
        self.files.insert(file, FileChange::Set(imports.into()));
    }

    /// `file`'s imports as an OLDER read saw them (the initial index, built
    /// from the tree as it was when the project opened): never replaces a
    /// change already queued for the file, which is newer.
    pub fn set_unless_queued(&mut self, file: PathBuf, imports: FileImports) {
        self.files
            .entry(file)
            .or_insert_with(|| FileChange::Set(imports.into()));
    }

    /// `file`'s imports — or its deletion, when the file set the batch is
    /// resolved against no longer lists it (decided in [`ImportGraph::apply`],
    /// against the same file set the resolution uses).
    pub fn set_if_listed(&mut self, file: PathBuf, imports: FileImports) {
        self.files
            .insert(file, FileChange::SetIfListed(imports.into()));
    }

    /// `file` is gone.
    pub fn remove(&mut self, file: PathBuf) {
        self.files.insert(file, FileChange::Removed);
    }

    /// What resolution reads besides the files' own imports changed — the
    /// file set, go.mod / pubspec metadata, tsconfig path maps: every file
    /// resolves again, against a newly built [`Resolver`].
    pub fn reresolve(&mut self) {
        self.reresolve = true;
    }

    /// The whole project is restated: drop every change queued so far and
    /// start the graph over from what is queued from now on.
    pub fn reset(&mut self) {
        self.files.clear();
        self.reset = true;
        self.reresolve = true;
    }

    /// Whether the resolver must be built anew for this batch.
    pub fn needs_new_resolver(&self) -> bool {
        self.reresolve
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && !self.reresolve && !self.reset
    }

    /// Ready `self` — the batch of a job that failed — to run once more,
    /// AHEAD of `later`, what was queued since (which must be applied after
    /// it): the changes `later` makes to the same files are dropped from it,
    /// since the later change wins and applying the earlier one first would
    /// be work thrown away; the re-resolve or reset it carried stays. Whether
    /// anything is left to run — nothing, when `later` restates the whole
    /// project (a reset), which supersedes all of it.
    pub fn retry_before(&mut self, later: &ImportBatch) -> bool {
        if later.reset {
            return false;
        }
        self.files.retain(|file, _| !later.files.contains_key(file));
        !self.is_empty()
    }
}

/// Whether edge lists `a` and `b` differ at most in the lines their
/// statements sit on (see [`Applied::structure_changed`]).
fn same_but_lines(a: &[Edge], b: &[Edge]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.target == y.target && x.specifier == y.specifier && x.kind == y.kind)
}

/// The project files `edges` import: a file's entry in
/// [`ImportGraph::scope_map`].
fn internal_targets(edges: &[Edge]) -> HashSet<&Path> {
    edges
        .iter()
        .filter_map(|e| match &e.target {
            Target::Internal(t) => Some(t.as_path()),
            _ => None,
        })
        .collect()
}

/// Whether edge lists `a` and `b` import the same project files (see
/// [`Applied::scope_changed`]).
fn same_scope(a: &[Edge], b: &[Edge]) -> bool {
    internal_targets(a) == internal_targets(b)
}

/// What [`ImportGraph::apply`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Some file's out-edges changed, or a file left the graph.
    pub changed: bool,
    /// Some file's out-edges changed in more than the lines their statements
    /// sit on — a target, a kind, an edge gained or lost — or a file left the
    /// graph: what the cycles and the rankings are drawn from. An edit above
    /// a file's imports moves only their lines, which is most edits.
    pub structure_changed: bool,
    /// Some file's set of imported PROJECT files changed: what
    /// [`ImportGraph::scope_map`] gives the project call graph. External and
    /// unresolved edges, edge kinds and lines do not count, and a file that
    /// imports no project file reads as one the graph does not hold.
    pub scope_changed: bool,
    /// How many files were resolved: the work the batch cost — the batch's
    /// own files, plus those that read a fact it changed.
    pub resolved: usize,
    /// The files whose resolution panicked, sorted, each with a short form of
    /// its panic's message ([`panic_note`]) for the status line. Each is left
    /// with its imports unresolved (every edge [`Target::Unresolved`]) and
    /// the rest of the batch lands all the same: a panic that repeats on the
    /// same input used to fail the whole job, and every job after it that
    /// carried it.
    pub unresolvable: Vec<(PathBuf, String)>,
}

/// A short form of a panic's message, for the status line: its first line,
/// cut at 80 characters. The whole message goes to stderr, from the panic
/// hook, as for any panic.
pub(crate) fn panic_note(panic: &(dyn std::any::Any + Send)) -> String {
    const MAX_CHARS: usize = 80;
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or(tr!("no message"));
    let line = message.lines().next().unwrap_or_default();
    let mut note: String = line.chars().take(MAX_CHARS).collect();
    if note.len() < line.len() {
        note.push('…');
    }
    note
}

/// The name of a file whose resolution panics — in tests only, where it is
/// how containing one file's panic ([`ImportGraph::apply`]) is proven.
#[cfg(test)]
pub(crate) const PANICKING_FILE: &str = "resolving_this_panics.py";

impl ImportGraph {
    /// Build the graph from every file's raw imports (no Rust items) and a
    /// resolver.
    pub fn build(
        raw: HashMap<PathBuf, Vec<RawImport>>,
        resolver: &Resolver,
        lang_of: impl Fn(&Path) -> Option<&'static str>,
    ) -> Self {
        let mut batch = ImportBatch::default();
        for (file, raw) in raw {
            batch.set(
                file,
                FileImports {
                    raw,
                    items: RustItems::new(),
                },
            );
        }
        let mut graph = ImportGraph::default();
        graph.apply(&batch, resolver, lang_of);
        graph
    }

    /// Apply `batch`: install every file it carries FIRST, then resolve once —
    /// never once per file, which re-resolved the whole project for each file
    /// of an initial index (quadratic in the file count).
    ///
    /// What resolves again is exactly what the batch can have changed:
    ///   * everything, when the batch re-resolves or resets (the resolver's
    ///     inputs changed);
    ///   * the batch's files whose imports changed — an edit below a file's
    ///     imports resolves nothing at all;
    ///   * the files whose resolution READ a Rust fact the batch changed:
    ///     a file's `mod`/`use` declarations or items, which another file's
    ///     `use` may follow (a crate root's `pub use` re-exports, a glob's
    ///     names, a declared module). Each file's reads are recorded as it
    ///     resolves ([`Consulted`]), so a new function in one module
    ///     re-resolves the few files that look names up through it, not every
    ///     Rust file of the project;
    ///   * and the C# files that use a namespace whose files the batch
    ///     changed ([`Namespaces`]): a namespace that came or went decides
    ///     whether a `using` is the project's. No other language resolves
    ///     through another file's contents.
    ///
    /// A file whose resolution panics is left with its imports unresolved
    /// ([`Applied::unresolvable`]); the panic goes no further than that file.
    pub fn apply(
        &mut self,
        batch: &ImportBatch,
        resolver: &Resolver,
        lang_of: impl Fn(&Path) -> Option<&'static str>,
    ) -> Applied {
        // A reset starts over from an empty graph; the one it replaces is kept
        // to the end, to tell whether the scope came back the same.
        let before = batch.reset.then(|| std::mem::take(self));
        self.apply_after(batch, resolver, lang_of, before.as_ref())
    }

    /// The graph `batch` makes of `base` — the graph the window shows, shared
    /// with it — and what applying it did ([`ImportGraph::apply`]). A batch
    /// that changes the graph applies to a copy of it (a key per file); one
    /// that restates the project starts from an empty graph and compares its
    /// scope with `base` itself, which the window keeps until the result
    /// replaces it — a copy of the graph the reset throws away used to be
    /// kept alive to the end of the job for that.
    pub fn applied_to(
        base: Arc<ImportGraph>,
        batch: &ImportBatch,
        resolver: &Resolver,
        lang_of: impl Fn(&Path) -> Option<&'static str>,
    ) -> (ImportGraph, Applied) {
        if batch.reset {
            let mut graph = ImportGraph::default();
            let applied = graph.apply_after(batch, resolver, lang_of, Some(&base));
            return (graph, applied);
        }
        let mut graph = Arc::unwrap_or_clone(base);
        let applied = graph.apply_after(batch, resolver, lang_of, None);
        (graph, applied)
    }

    /// [`ImportGraph::apply`] to `self` as the batch finds it — emptied, for
    /// a batch that resets, with `before` the graph that reset replaces.
    fn apply_after(
        &mut self,
        batch: &ImportBatch,
        resolver: &Resolver,
        lang_of: impl Fn(&Path) -> Option<&'static str>,
        before: Option<&ImportGraph>,
    ) -> Applied {
        let mut changed = false;
        let mut structure_changed = false;
        let mut scope_changed = false;
        // Go and Java package imports reach their packages' files as the
        // project has them now: a file come into a package, or gone from it,
        // is in the scope of every file importing it, or out.
        let held = before.map_or(&self.packages, |before| &before.packages);
        let packages_changed =
            !Arc::ptr_eq(held, &resolver.packages) && **held != *resolver.packages;
        self.packages = resolver.packages.clone();
        if let Some(before) = before {
            changed = !before.out.is_empty();
            structure_changed = changed;
        }
        // The files whose Rust facts — what other files resolve through —
        // changed, appeared or went; the C# namespaces whose files did.
        let mut facts_changed: Vec<PathBuf> = Vec::new();
        let mut namespaces_changed: HashSet<PathBuf> = HashSet::new();
        let mut dirty: Vec<PathBuf> = Vec::new();
        for (file, change) in &batch.files {
            let queued = match change {
                FileChange::Set(queued) => Some(queued),
                FileChange::SetIfListed(queued) => resolver.has_file(file).then_some(queued),
                FileChange::Removed => None,
            };
            let Some(queued) = queued else {
                self.retract(file);
                let left = self.out.remove(file);
                changed |= left.is_some();
                structure_changed |= left.is_some();
                scope_changed |= left.is_some_and(|edges| !internal_targets(&edges).is_empty());
                self.raw.remove(file);
                self.set_reads(file, Vec::new());
                if self.ctx.remove(file) {
                    facts_changed.push(file.clone());
                }
                namespaces_changed.extend(self.namespaces.set(file, Vec::new()));
                continue;
            };
            if lang_of(file) == Some("rust")
                && self
                    .ctx
                    .set(file, RustFacts::of(&queued.raw, queued.items.clone()))
            {
                facts_changed.push(file.clone());
            }
            let declares: Vec<PathBuf> = match lang_of(file) {
                Some("csharp") => queued
                    .raw
                    .iter()
                    .filter(|r| r.is_mod_decl)
                    .map(|r| resolver.namespace_node(&r.module))
                    .collect(),
                Some("java" | "kotlin" | "scala") => queued
                    .raw
                    .iter()
                    .filter(|r| r.is_mod_decl)
                    .map(|r| resolver.package_node(&r.module))
                    .collect(),
                // Each module an Elixir file defines, wherever it is.
                Some("elixir") => queued
                    .raw
                    .iter()
                    .filter(|r| r.is_mod_decl)
                    .map(|r| resolver.package_node(&r.module))
                    .collect(),
                _ => Vec::new(),
            };
            namespaces_changed.extend(self.namespaces.set(file, declares));
            let same = self.out.contains_key(file)
                && self
                    .raw
                    .get(file)
                    .is_some_and(|held| Arc::ptr_eq(held, &queued.raw) || **held == *queued.raw);
            if !same {
                self.raw.insert(file.clone(), queued.raw.clone());
                dirty.push(file.clone());
            }
        }
        let mut scope: Vec<PathBuf> = if batch.reset || batch.reresolve {
            self.raw.keys().cloned().collect()
        } else {
            let readers = facts_changed
                .iter()
                .filter_map(|f| self.readers.get(f))
                .flat_map(|readers| readers.iter().cloned());
            let users = self.namespace_users(&namespaces_changed, resolver, &lang_of);
            dirty
                .into_iter()
                .chain(readers)
                .chain(users)
                .filter(|f| self.raw.contains_key(f))
                .collect()
        };
        // Deterministic reverse-index order, and each file once.
        scope.sort();
        scope.dedup();
        let mut unresolvable = Vec::new();
        for file in &scope {
            // A panic stays with the file that raised it (a resolver bug
            // some input trips): it keeps its imports, unresolved, and the
            // rest of the batch lands. Nothing is left half-done by one —
            // resolving only reads the graph.
            let resolved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.resolve_file(file, resolver, &lang_of)
            }));
            let (edges, reads) = resolved.unwrap_or_else(|panic| {
                unresolvable.push((file.clone(), panic_note(&*panic)));
                (self.unresolved_edges(file, lang_of(file)), Vec::new())
            });
            self.set_reads(file, reads);
            let held = self.out.get(file).map(|held| held.as_slice());
            if held != Some(edges.as_slice()) {
                structure_changed |= !held.is_some_and(|held| same_but_lines(held, &edges));
                scope_changed |= !same_scope(held.unwrap_or_default(), &edges);
                self.install(file.clone(), edges);
                changed = true;
            }
        }
        // A namespace's files are what its `using`s reach — the files that
        // count on them, and their importers' scopes, moved with them.
        let namespaces_moved = match before {
            Some(before) => before.namespaces != self.namespaces,
            None => !namespaces_changed.is_empty(),
        };
        // A reset resolved every file against an empty graph: its scope is
        // judged against the graph the reset replaced.
        if let Some(before) = before {
            scope_changed = !self.same_scope_as(before);
        }
        changed |= packages_changed || namespaces_moved;
        structure_changed |= namespaces_moved;
        scope_changed |= packages_changed || namespaces_moved;
        Applied {
            changed,
            structure_changed,
            scope_changed,
            resolved: scope.len(),
            unresolvable,
        }
    }

    /// The C# files with a `using` of one of the namespaces `changed`
    /// (nodes), or of a type in one: what resolves differently once the
    /// namespace's files changed — a namespace that came or went, a type's
    /// file that joined or left it.
    fn namespace_users(
        &self,
        changed: &HashSet<PathBuf>,
        resolver: &Resolver,
        lang_of: &impl Fn(&Path) -> Option<&'static str>,
    ) -> Vec<PathBuf> {
        if changed.is_empty() {
            return Vec::new();
        }
        // A `using`'s name, and the namespace it is a type of, if it is one.
        fn names(r: &RawImport) -> impl Iterator<Item = &str> {
            let parent = r.module.rsplit_once('.').map(|(namespace, _)| namespace);
            [Some(r.module.as_str()), parent].into_iter().flatten()
        }
        // A JVM import's packages: each prefix of its name.
        fn packages(r: &RawImport) -> impl Iterator<Item = &str> {
            let name = r.module.as_str();
            name.match_indices('.').map(move |(at, _)| &name[..at])
        }
        self.raw
            .iter()
            .filter(|(file, raws)| match lang_of(file) {
                Some("csharp") => raws
                    .iter()
                    .filter(|r| !r.is_mod_decl)
                    .flat_map(names)
                    .any(|name| changed.contains(&resolver.namespace_node(name))),
                Some("java" | "kotlin" | "scala") => raws
                    .iter()
                    .filter(|r| !r.is_mod_decl)
                    .flat_map(packages)
                    .any(|name| changed.contains(&resolver.package_node(name))),
                Some("elixir") => raws
                    .iter()
                    .filter(|r| !r.is_mod_decl)
                    .any(|r| changed.contains(&resolver.package_node(&r.module))),
                _ => false,
            })
            .map(|(file, _)| file.clone())
            .collect()
    }

    /// Whether `self` and `other` give every file the same imported project
    /// files ([`Applied::scope_changed`]).
    fn same_scope_as(&self, other: &ImportGraph) -> bool {
        self.out
            .keys()
            .chain(other.out.keys())
            .all(|file| same_scope(self.imports(file), other.imports(file)))
    }

    /// `file`'s imports as edges that resolve nowhere: what a file whose
    /// resolution panicked is left with — its imports on show, none of them
    /// followed. Its declarations (`lang`'s, a Java package) are no edges.
    fn unresolved_edges(&self, file: &Path, lang: Option<&str>) -> Vec<Edge> {
        let raws = self.raw.get(file).map(|r| r.as_slice()).unwrap_or(&[]);
        raws.iter()
            .filter(|r| !lang.is_some_and(|lang| is_declaration(lang, r)))
            .map(|r| Edge {
                target: Target::Unresolved(r.module.clone()),
                specifier: r.module.clone(),
                line: r.line,
                kind: if r.is_mod_decl {
                    EdgeKind::ModDecl
                } else {
                    EdgeKind::Use
                },
            })
            .collect()
    }

    /// `file`'s out-edges, and the files whose Rust facts resolving them read.
    fn resolve_file(
        &self,
        file: &Path,
        resolver: &Resolver,
        lang_of: &impl Fn(&Path) -> Option<&'static str>,
    ) -> (Vec<Edge>, Vec<PathBuf>) {
        #[cfg(test)]
        if file.file_name().is_some_and(|name| name == PANICKING_FILE) {
            panic!("a planted resolver panic");
        }
        let Some(lang) = lang_of(file) else {
            return (Vec::new(), Vec::new());
        };
        let consulted = Consulted::new(&self.ctx);
        let raws = self.raw.get(file).map(|r| r.as_slice()).unwrap_or(&[]);
        // A Java file's own `package` line says where its source root is.
        let package = matches!(lang, "java" | "kotlin" | "scala")
            .then(|| raws.iter().find(|r| r.is_mod_decl))
            .flatten()
            .map(|r| r.module.as_str());
        let declared = Declared {
            package,
            namespaces: Some(&self.namespaces),
        };
        let edges = raws
            .iter()
            .filter(|r| !is_declaration(lang, r))
            .map(|r| {
                let (target, inferred) = if lang == "rust" {
                    resolver.resolve_rust_inferred(r, file, &consulted)
                } else {
                    let target = resolver.resolve_with(r, file, lang, &self.ctx, &declared);
                    (target, false)
                };
                let kind = if r.is_mod_decl {
                    EdgeKind::ModDecl
                } else if lang == "rust" {
                    resolver.rust_use_kind(r, file, &target, inferred)
                } else {
                    EdgeKind::Use
                };
                Edge {
                    target,
                    specifier: r.module.clone(),
                    line: r.line,
                    kind,
                }
            })
            // A Rust path naming the file itself (`use self::Item`, a test
            // module's `use super::*`, a local enum's `use Kind::*`), or a
            // Java static import of the file's own class, is not a
            // dependency.
            .filter(|e| {
                !(matches!(lang, "rust" | "java" | "kotlin" | "scala" | "elixir")
                    && e.target == Target::Internal(file.to_path_buf()))
            })
            .collect();
        (edges, consulted.into_read())
    }

    /// Record the files `file`'s resolution read (`reads`, empty to forget
    /// them), keeping `readers` its exact reverse.
    fn set_reads(&mut self, file: &Path, reads: Vec<PathBuf>) {
        if self.reads.get(file).map(|held| held.as_slice()) == Some(reads.as_slice()) {
            return;
        }
        if let Some(old) = self.reads.remove(file) {
            for read in old.iter() {
                let emptied = match self.readers.get_mut(read) {
                    Some(readers) => {
                        Arc::make_mut(readers).remove(file);
                        readers.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    self.readers.remove(read);
                }
            }
        }
        if reads.is_empty() {
            return;
        }
        for read in &reads {
            Arc::make_mut(self.readers.entry(read.clone()).or_default()).insert(file.to_path_buf());
        }
        self.reads.insert(file.to_path_buf(), Arc::new(reads));
    }

    /// Insert/replace `file`'s edges and update the reverse index. A file that
    /// imports the same target twice (`use a::Foo; use a::Bar;`) contributes a
    /// single reverse entry, so `fan_in` counts distinct importers.
    fn install(&mut self, file: PathBuf, edges: Vec<Edge>) {
        self.retract(&file);
        let mut seen: HashSet<&Path> = HashSet::new();
        for e in &edges {
            if let Target::Internal(t) = &e.target
                && seen.insert(t.as_path())
            {
                Arc::make_mut(self.rev.entry(t.clone()).or_default()).push(file.clone());
            }
        }
        self.out.insert(file, Arc::new(edges));
    }

    /// Remove `file`'s contribution to the reverse index (before re-inserting).
    fn retract(&mut self, file: &Path) {
        let Some(old) = self.out.get(file) else {
            return;
        };
        for e in old.iter() {
            let Target::Internal(t) = &e.target else {
                continue;
            };
            let emptied = match self.rev.get_mut(t) {
                Some(importers) => {
                    Arc::make_mut(importers).retain(|p| p != file);
                    importers.is_empty()
                }
                None => false,
            };
            if emptied {
                self.rev.remove(t);
            }
        }
    }

    /// Files this file imports (internal + external + unresolved), in source order.
    pub fn imports(&self, file: &Path) -> &[Edge] {
        self.out.get(file).map(|e| e.as_slice()).unwrap_or(&[])
    }

    /// Files that import this file (internal edges only), sorted for stable
    /// display — a Go or Java file's, those that import its package; a C#
    /// file's, those that use a namespace it declares.
    pub fn importers(&self, file: &Path) -> Vec<PathBuf> {
        let mut v = self
            .rev
            .get(file)
            .map(|importers| importers.to_vec())
            .unwrap_or_default();
        if is_package_source(file)
            && let Some(package) = file.parent().and_then(|dir| self.rev.get(dir))
        {
            v.extend(package.iter().cloned());
        }
        for namespace in self.namespaces.of(file) {
            if let Some(users) = self.rev.get(namespace) {
                v.extend(users.iter().filter(|u| u.as_path() != file).cloned());
            }
        }
        v.sort();
        v.dedup();
        v
    }

    /// Each Go and Java package directory of the project, with its files
    /// (sorted, a Go package's tests aside), as the resolver last applied
    /// found them: a Go import, or a Java `*` import, names a package — a
    /// directory — and reaches every file of it, where most languages' names
    /// one file. (A C# `using` reaches its namespace's files likewise, which
    /// the graph keeps itself: [`Self::imported_files`] reaches them too.)
    pub fn packages(&self) -> Arc<HashMap<PathBuf, Vec<PathBuf>>> {
        self.packages.clone()
    }

    /// The files a package node reaches: a Go or Java package directory's
    /// (from `packages`, [`Self::packages`]), or a C# namespace's.
    fn package_files<'a>(
        &'a self,
        node: &Path,
        packages: &'a HashMap<PathBuf, Vec<PathBuf>>,
    ) -> Option<&'a [PathBuf]> {
        packages
            .get(node)
            .map(Vec::as_slice)
            .or_else(|| self.namespaces.files(node))
    }

    /// The project files `file` imports, each once: an import's file, or the
    /// files of a package it imports (a Go or Java package from `packages`,
    /// [`Self::packages`]; a C# namespace).
    pub fn imported_files(
        &self,
        file: &Path,
        packages: &HashMap<PathBuf, Vec<PathBuf>>,
    ) -> Vec<PathBuf> {
        let mut seen: HashSet<&Path> = HashSet::new();
        let mut files = Vec::new();
        for e in self.imports(file) {
            let Target::Internal(t) = &e.target else {
                continue;
            };
            let reached = self
                .package_files(t, packages)
                .unwrap_or(std::slice::from_ref(t));
            for f in reached {
                if f != file && seen.insert(f.as_path()) {
                    files.push(f.clone());
                }
            }
        }
        files
    }

    /// The first file of the package `node` — a Go or Java package's
    /// directory, a C# namespace's node — by path: what a click on the
    /// package opens. `None` when `node` is no package of the graph's.
    pub fn package_file(&self, node: &Path) -> Option<PathBuf> {
        self.package_files(node, &self.packages)?.first().cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.out.values().all(|e| e.is_empty())
    }

    pub fn file_count(&self) -> usize {
        self.raw.len()
    }

    /// Every project-internal file that appears in the graph (as a source of
    /// imports or a target of one), sorted for stable display.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut set: HashSet<PathBuf> = self.out.keys().cloned().collect();
        // A package a file imports (a Go or Java package, a C# namespace) as
        // its files: a directory or a namespace is no file of the graph, and
        // its files, importing nothing, are there by being imported.
        for target in self.rev.keys() {
            match self.package_files(target, &self.packages) {
                Some(files) => set.extend(files.iter().cloned()),
                None => {
                    set.insert(target.clone());
                }
            }
        }
        let mut v: Vec<PathBuf> = set.into_iter().collect();
        v.sort();
        v
    }

    /// Number of distinct internal file→file edges (two imports of the same
    /// target from one file count once).
    pub fn internal_edge_count(&self) -> usize {
        let mut seen: HashSet<(&Path, &Path)> = HashSet::new();
        for (src, edges) in &self.out {
            for e in edges.iter() {
                if let Target::Internal(t) = &e.target {
                    seen.insert((src.as_path(), t.as_path()));
                }
            }
        }
        seen.len()
    }

    /// Unique external packages depended on anywhere, sorted.
    pub fn external_packages(&self) -> Vec<String> {
        let mut set: HashSet<&str> = HashSet::new();
        for edges in self.out.values() {
            for e in edges.iter() {
                if let Target::External(name) = &e.target {
                    set.insert(name.as_str());
                }
            }
        }
        let mut v: Vec<String> = set.into_iter().map(str::to_string).collect();
        v.sort();
        v
    }

    /// Each file mapped to the set of internal files it imports — the scope the
    /// project call graph resolves called names within.
    pub fn scope_map(&self) -> HashMap<PathBuf, HashSet<PathBuf>> {
        // A package import's package as its files: the call graph matches a
        // call's file against the files in its scope, and a directory (or a
        // namespace) matched none.
        let packages = self.packages();
        self.out
            .keys()
            .map(|file| {
                let targets: HashSet<PathBuf> =
                    self.imported_files(file, &packages).into_iter().collect();
                (file.clone(), targets)
            })
            .collect()
    }

    /// Number of internal files that import `file` (its fan-in) — a Go or
    /// Java file's, those that import its package; a C# file's, those that
    /// use its namespaces.
    pub fn fan_in(&self, file: &Path) -> usize {
        if is_package_source(file) || !self.namespaces.of(file).is_empty() {
            return self.importers(file).len();
        }
        self.rev.get(file).map(|v| v.len()).unwrap_or(0)
    }

    /// Number of distinct internal files `file` imports (its fan-out).
    pub fn fan_out(&self, file: &Path) -> usize {
        self.out
            .get(file)
            .map(|edges| {
                let mut set: HashSet<&Path> = HashSet::new();
                for e in edges.iter() {
                    if let Target::Internal(t) = &e.target {
                        set.insert(t.as_path());
                    }
                }
                set.len()
            })
            .unwrap_or(0)
    }

    /// Import cycles among project-internal files (each a strongly connected
    /// component of size > 1, or a self-import), via Tarjan's algorithm.
    /// Dependency edges only ([`EdgeKind::is_dependency`]): a Rust `mod`
    /// declaration is ownership, a `pub use` re-export is a facade, and a
    /// child's glob of an enclosing module is the module tree's namespace —
    /// the normal shape of a module tree, not a cycle.
    pub fn cycles(&self) -> Vec<Vec<PathBuf>> {
        Tarjan::new(self).run()
    }
}

/// Tarjan's strongly-connected-components over the internal DEPENDENCY edges
/// ([`EdgeKind::is_dependency`]), for cycle detection. Iterative (an explicit
/// DFS stack), so a very deep import chain cannot overflow the call stack.
struct Tarjan<'a> {
    graph: &'a ImportGraph,
    index: HashMap<PathBuf, usize>,
    low: HashMap<PathBuf, usize>,
    on_stack: HashSet<PathBuf>,
    stack: Vec<PathBuf>,
    next: usize,
    out: Vec<Vec<PathBuf>>,
}

impl<'a> Tarjan<'a> {
    fn new(graph: &'a ImportGraph) -> Self {
        Tarjan {
            graph,
            index: HashMap::new(),
            low: HashMap::new(),
            on_stack: HashSet::new(),
            stack: Vec::new(),
            next: 0,
            out: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<Vec<PathBuf>> {
        let mut nodes: Vec<PathBuf> = self.graph.out.keys().cloned().collect();
        nodes.sort();
        for n in nodes {
            if !self.index.contains_key(&n) {
                self.connect(&n);
            }
        }
        self.out
    }

    fn internal_targets(&self, node: &Path) -> Vec<PathBuf> {
        self.graph
            .out
            .get(node)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|e| e.kind.is_dependency())
                    .filter_map(|e| match &e.target {
                        Target::Internal(p) => Some(p.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Assign `node` its discovery index and push it onto the SCC stack.
    fn enter(&mut self, node: PathBuf) -> PathBuf {
        self.index.insert(node.clone(), self.next);
        self.low.insert(node.clone(), self.next);
        self.next += 1;
        self.stack.push(node.clone());
        self.on_stack.insert(node.clone());
        node
    }

    /// Visit the SCC(s) reachable from `start`. Explicit-stack (not recursive) so
    /// a very deep import chain can't overflow the call stack.
    fn connect(&mut self, start: &Path) {
        // A suspended DFS frame: the node, its internal targets, and how far we've
        // walked them.
        struct Frame {
            v: PathBuf,
            targets: Vec<PathBuf>,
            next: usize,
        }
        let first = self.enter(start.to_path_buf());
        let mut call: Vec<Frame> = vec![Frame {
            targets: self.internal_targets(&first),
            v: first,
            next: 0,
        }];

        while let Some(top) = call.last_mut() {
            if top.next < top.targets.len() {
                let w = top.targets[top.next].clone();
                let v = top.v.clone();
                top.next += 1;
                if !self.index.contains_key(&w) {
                    // "Recurse" into w by pushing a new frame.
                    let node = self.enter(w);
                    call.push(Frame {
                        targets: self.internal_targets(&node),
                        v: node,
                        next: 0,
                    });
                } else if self.on_stack.contains(&w) {
                    let iw = self.index[&w];
                    let lv = self.low[&v];
                    self.low.insert(v, lv.min(iw));
                }
                continue;
            }

            // v's neighbors are exhausted: pop its SCC if it is a root.
            let v = top.v.clone();
            let self_loop = top.targets.iter().any(|t| t == &v);
            if self.low[&v] == self.index[&v] {
                let mut comp = Vec::new();
                while let Some(w) = self.stack.pop() {
                    self.on_stack.remove(&w);
                    comp.push(w.clone());
                    if w == v {
                        break;
                    }
                }
                // A real cycle is a multi-node SCC or a self-import.
                if comp.len() > 1 || self_loop {
                    comp.sort();
                    self.out.push(comp);
                }
            }
            call.pop();
            // Propagate v's low-link up to its parent (the new top).
            if let Some(parent) = call.last() {
                let pv = parent.v.clone();
                let lv = self.low[&v];
                let lp = self.low[&pv];
                self.low.insert(pv, lp.min(lv));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The view tree (synchronous; expands straight from the in-memory graph)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// What this file imports.
    Imports,
    /// What imports this file.
    Importers,
}

impl Dir {
    pub fn label(self) -> &'static str {
        match self {
            Dir::Imports => tr!("Imports"),
            Dir::Importers => tr!("Importers"),
        }
    }

    pub fn toggled(self) -> Dir {
        match self {
            Dir::Imports => Dir::Importers,
            Dir::Importers => Dir::Imports,
        }
    }
}

/// Whether `file` is a package's own source — what an import of its
/// directory reaches: a Go file (a `_test.go` file is the package's test),
/// or a Java class, which a `*` import of its package reaches.
fn is_package_source(file: &Path) -> bool {
    match file.extension().and_then(|e| e.to_str()) {
        Some("go") => !file
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with("_test.go")),
        Some("java" | "kt" | "scala") => true,
        _ => false,
    }
}

/// A node in the import view tree.
#[derive(Debug, Clone)]
pub struct INode {
    pub target: Target,
    /// Display label (file name, external package, or unresolved specifier).
    pub label: String,
    /// Secondary text (relative path for internal, "external"/specifier otherwise).
    pub detail: String,
    /// The import site line in the *parent* file, for navigation (0 for the root).
    pub line: usize,
    pub depth: usize,
    pub parent: Option<usize>,
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    /// The target already appears on the ancestor path — a cyclic leaf.
    pub cyclic: bool,
    /// Children that exist but were not added because the tree reached
    /// [`MAX_NODES`] ("N more not shown").
    pub hidden: usize,
}

impl crate::graph::tree::TreeNode for INode {
    fn parent(&self) -> Option<usize> {
        self.parent
    }
    fn children(&self) -> Option<&[usize]> {
        self.children.as_deref()
    }
    fn expanded(&self) -> bool {
        self.expanded
    }
}

/// A lazily-expanded view of the import graph around one focus file. Unlike the
/// call tree it expands synchronously (the whole graph is already in memory).
#[derive(Debug, Clone)]
pub struct ImportTree {
    pub direction: Dir,
    pub root_path: PathBuf,
    pub root_name: String,
    pub full: bool,
    nodes: Vec<INode>,
    roots: Vec<usize>,
}

/// Cap on tree size so "expand all" on a hub file can't fan out unboundedly.
pub const MAX_NODES: usize = 2000;

impl ImportTree {
    /// Build a tree rooted at `focus`, expanded one level.
    pub fn new(graph: &ImportGraph, root: &Path, focus: PathBuf, direction: Dir) -> Self {
        let root_name = rel_name(root, &focus);
        let mut tree = ImportTree {
            direction,
            root_path: focus.clone(),
            root_name,
            full: false,
            nodes: Vec::new(),
            roots: Vec::new(),
        };
        let id = tree.push(INode {
            target: Target::Internal(focus.clone()),
            label: rel_name(root, &focus),
            detail: rel_path(root, &focus),
            line: 0,
            depth: 0,
            parent: None,
            children: None,
            expanded: false,
            cyclic: false,
            hidden: 0,
        });
        tree.roots.push(id);
        tree.expand(id, graph, root);
        tree
    }

    fn push(&mut self, node: INode) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        id
    }

    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    pub fn node(&self, id: usize) -> &INode {
        &self.nodes[id]
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Whether `id` can be expanded to reveal children (internal, not yet
    /// expanded, not cyclic).
    pub fn expandable(&self, id: usize) -> bool {
        self.get(id)
            .is_some_and(|n| matches!(n.target, Target::Internal(_)) && !n.cyclic)
    }

    fn is_ancestor(&self, parent: Option<usize>, path: &Path) -> bool {
        crate::graph::tree::any_on_path(
            &self.nodes,
            parent,
            |n| matches!(&n.target, Target::Internal(t) if t == path),
        )
    }

    /// Node `id`, or `None` for a stale id (from a tree since replaced).
    pub fn get(&self, id: usize) -> Option<&INode> {
        self.nodes.get(id)
    }

    /// "N more not shown" for a node the size cap cut short.
    pub fn hidden_note(&self, id: usize) -> Option<String> {
        let hidden = self.get(id)?.hidden;
        (hidden > 0).then(|| {
            trn!(
                "1 more not shown (tree limit {limit})",
                "{n} more not shown (tree limit {limit})",
                hidden,
                limit = MAX_NODES
            )
        })
    }

    /// Expand `id` in place, pulling its children from the graph. Idempotent. A
    /// cyclic node is a leaf (expanding it would re-materialize the cycle), so it
    /// gets no children.
    pub fn expand(&mut self, id: usize, graph: &ImportGraph, root: &Path) {
        self.expand_limited(id, graph, root, usize::MAX);
    }

    /// Expand `id`, adding at most `max_new` children (so a caller enforcing a
    /// total node budget can't be overshot by one hub's fan-out).
    fn expand_limited(&mut self, id: usize, graph: &ImportGraph, root: &Path, max_new: usize) {
        if id >= self.nodes.len() {
            return; // a stale id from a tree since replaced
        }
        if self.nodes[id].children.is_some() {
            self.nodes[id].expanded = true;
            return;
        }
        if self.nodes[id].cyclic {
            self.nodes[id].children = Some(Vec::new());
            return;
        }
        let Target::Internal(path) = self.nodes[id].target.clone() else {
            self.nodes[id].children = Some(Vec::new());
            return;
        };
        let depth = self.nodes[id].depth + 1;
        let mut children = self.children_of(&path, graph, root, id, depth);
        let hidden = children.len().saturating_sub(max_new);
        children.truncate(max_new);
        let ids: Vec<usize> = children.into_iter().map(|c| self.push(c)).collect();
        self.nodes[id].children = Some(ids);
        self.nodes[id].expanded = true;
        self.nodes[id].hidden = hidden;
    }

    /// Build (but don't insert) the child nodes for internal file `path`.
    fn children_of(
        &self,
        path: &Path,
        graph: &ImportGraph,
        root: &Path,
        parent: usize,
        depth: usize,
    ) -> Vec<INode> {
        let mut out = Vec::new();
        // A package (a Go or Java import names its directory, a C# `using`
        // its namespace): its files, each expanded from there, in either
        // direction.
        if graph.imports(path).is_empty()
            && let Some(files) = graph.package_files(path, &graph.packages)
        {
            for f in files {
                let cyclic = self.is_ancestor(Some(parent), f);
                out.push(INode {
                    target: Target::Internal(f.clone()),
                    label: rel_name(root, f),
                    detail: rel_path(root, f),
                    line: 0,
                    depth,
                    parent: Some(parent),
                    children: None,
                    expanded: false,
                    cyclic,
                    hidden: 0,
                });
            }
            return out;
        }
        match self.direction {
            Dir::Imports => {
                let mut seen_ext: HashSet<String> = HashSet::new();
                let mut seen_int: HashSet<PathBuf> = HashSet::new();
                for e in graph.imports(path) {
                    match &e.target {
                        Target::Internal(t) => {
                            if !seen_int.insert(t.clone()) {
                                continue; // dedup repeated imports of the same file
                            }
                            let cyclic = self.is_ancestor(Some(parent), t);
                            out.push(INode {
                                target: Target::Internal(t.clone()),
                                label: rel_name(root, t),
                                detail: rel_path(root, t),
                                line: e.line,
                                depth,
                                parent: Some(parent),
                                children: None,
                                expanded: false,
                                cyclic,
                                hidden: 0,
                            });
                        }
                        Target::External(name) => {
                            if seen_ext.insert(name.clone()) {
                                out.push(leaf(e, name.clone(), tr!("external"), depth, parent));
                            }
                        }
                        Target::Unresolved(spec) => {
                            if seen_ext.insert(format!("?{spec}")) {
                                out.push(leaf(e, spec.clone(), tr!("unresolved"), depth, parent));
                            }
                        }
                    }
                }
            }
            Dir::Importers => {
                for t in graph.importers(path) {
                    let cyclic = self.is_ancestor(Some(parent), &t);
                    out.push(INode {
                        target: Target::Internal(t.clone()),
                        label: rel_name(root, &t),
                        detail: rel_path(root, &t),
                        line: 0,
                        depth,
                        parent: Some(parent),
                        children: None,
                        expanded: false,
                        cyclic,
                        hidden: 0,
                    });
                }
            }
        }
        out
    }

    /// Collapse/expand a node whose children are already loaded, else expand it.
    /// A no-op for a stale id.
    pub fn toggle(&mut self, id: usize, graph: &ImportGraph, root: &Path) {
        let Some(n) = self.nodes.get_mut(id) else {
            return;
        };
        if n.children.is_some() {
            n.expanded = !n.expanded;
        } else {
            self.expand(id, graph, root);
        }
    }

    /// Recursively expand every internal node, capping the total at `MAX_NODES`
    /// (a hub's fan-out is truncated rather than overshooting the cap).
    pub fn expand_all(&mut self, graph: &ImportGraph, root: &Path) {
        self.full = true;
        let mut i = 0;
        while i < self.nodes.len() {
            let remaining = MAX_NODES.saturating_sub(self.nodes.len());
            if remaining == 0 {
                break;
            }
            if self.expandable(i) && self.nodes[i].children.is_none() {
                self.expand_limited(i, graph, root, remaining);
            }
            i += 1;
        }
    }

    /// Node ids in display order (depth-first, children only under expanded nodes).
    pub fn visible(&self) -> Vec<usize> {
        crate::graph::tree::visible(&self.nodes, &self.roots)
    }
}

fn leaf(e: &Edge, label: String, detail: &str, depth: usize, parent: usize) -> INode {
    INode {
        target: e.target.clone(),
        label,
        detail: detail.to_string(),
        line: e.line,
        depth,
        parent: Some(parent),
        children: Some(Vec::new()),
        expanded: false,
        cyclic: false,
        hidden: 0,
    }
}

/// File name for display (`client.rs`).
fn rel_name(_root: &Path, path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Path relative to the project root for the secondary label (`src/lsp`).
fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .parent()
        .map(clew_core::rel_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

/// Whether `top` is a module of Python's standard library: the names
/// `sys.stdlib_module_names` lists (3.13, the private `_` ones left out),
/// and the ones recent releases removed that older projects still import.
fn python_stdlib(top: &str) -> bool {
    PY_STDLIB.binary_search(&top).is_ok()
}

/// Sorted, for [`python_stdlib`].
#[rustfmt::skip]
const PY_STDLIB: &[&str] = &[
    "__future__", "abc", "aifc", "antigravity", "argparse", "array", "ast", "asynchat", "asyncio",
    "asyncore", "atexit", "audioop", "base64", "bdb", "binascii", "bisect", "builtins", "bz2",
    "cProfile", "calendar", "cgi", "cgitb", "chunk", "cmath", "cmd", "code", "codecs", "codeop",
    "collections", "colorsys", "compileall", "concurrent", "configparser", "contextlib",
    "contextvars", "copy", "copyreg", "crypt", "csv", "ctypes", "curses", "dataclasses",
    "datetime", "dbm", "decimal", "difflib", "dis", "distutils", "doctest", "email", "encodings",
    "ensurepip", "enum", "errno", "faulthandler", "fcntl", "filecmp", "fileinput", "fnmatch",
    "fractions", "ftplib", "functools", "gc", "genericpath", "getopt", "getpass", "gettext",
    "glob", "graphlib", "grp", "gzip", "hashlib", "heapq", "hmac", "html", "http", "idlelib",
    "imaplib", "imghdr", "imp", "importlib", "inspect", "io", "ipaddress", "itertools", "json",
    "keyword", "lib2to3", "linecache", "locale", "logging", "lzma", "mailbox", "mailcap",
    "marshal", "math", "mimetypes", "mmap", "modulefinder", "msilib", "msvcrt", "multiprocessing",
    "netrc", "nis", "nntplib", "nt", "ntpath", "nturl2path", "numbers", "opcode", "operator",
    "optparse", "os", "ossaudiodev", "pathlib", "pdb", "pickle", "pickletools", "pipes",
    "pkgutil", "platform", "plistlib", "poplib", "posix", "posixpath", "pprint", "profile",
    "pstats", "pty", "pwd", "py_compile", "pyclbr", "pydoc", "pydoc_data", "pyexpat", "queue",
    "quopri", "random", "re", "readline", "reprlib", "resource", "rlcompleter", "runpy", "sched",
    "secrets", "select", "selectors", "shelve", "shlex", "shutil", "signal", "site", "smtpd",
    "smtplib", "sndhdr", "socket", "socketserver", "spwd", "sqlite3", "sre_compile",
    "sre_constants", "sre_parse", "ssl", "stat", "statistics", "string", "stringprep", "struct",
    "subprocess", "sunau", "symtable", "sys", "sysconfig", "syslog", "tabnanny", "tarfile",
    "telnetlib", "tempfile", "termios", "textwrap", "this", "threading", "time", "timeit",
    "tkinter", "token", "tokenize", "tomllib", "trace", "traceback", "tracemalloc", "tty",
    "turtle", "turtledemo", "types", "typing", "unicodedata", "unittest", "urllib", "uu", "uuid",
    "venv", "warnings", "wave", "weakref", "webbrowser", "winreg", "winsound", "wsgiref",
    "xdrlib", "xml", "xmlrpc", "zipapp", "zipfile", "zipimport", "zlib", "zoneinfo",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn ri(module: &str) -> RawImport {
        RawImport {
            module: module.into(),
            line: 1,
            is_mod_decl: false,
        }
    }

    // --- extraction ---

    #[test]
    fn extracts_rust_mod_and_use() {
        let src = "mod alpha;\nmod inline { fn x() {} }\nuse crate::beta::Thing;\nuse std::collections::HashMap;\nuse super::gamma::{a, b};\n";
        let imports = imports_of(src, "rust");
        let mods: Vec<&RawImport> = imports.iter().filter(|i| i.is_mod_decl).collect();
        assert_eq!(
            mods.len(),
            1,
            "only the file-mod, not the inline mod: {imports:?}"
        );
        assert_eq!(mods[0].module, "alpha");
        let uses: Vec<&str> = imports
            .iter()
            .filter(|i| !i.is_mod_decl)
            .map(|i| i.module.as_str())
            .collect();
        assert!(uses.contains(&"crate::beta::Thing"), "{uses:?}");
        assert!(uses.contains(&"std::collections::HashMap"), "{uses:?}");
        assert!(
            uses.contains(&"super::gamma::a") && uses.contains(&"super::gamma::b"),
            "a grouped import yields each path it names: {uses:?}"
        );
    }

    #[test]
    fn extracts_python_imports() {
        let src = "import os\nimport a.b.c as x\nfrom .sibling import thing\nfrom ..pkg import other\nfrom mod import y\n";
        let extracted = imports_of(src, "python");
        let mods: Vec<&str> = extracted.iter().map(|i| i.module.as_str()).collect();
        assert!(mods.contains(&"os"), "{mods:?}");
        assert!(mods.contains(&"a.b.c"), "{mods:?}");
        // A `from` import keeps the name it imports beside its module.
        assert!(mods.contains(&".sibling:thing"), "{mods:?}");
        assert!(mods.contains(&"..pkg:other"), "{mods:?}");
        assert!(mods.contains(&"mod:y"), "{mods:?}");
    }

    #[test]
    fn extracts_js_imports_and_reexports() {
        let src = "import a from './a';\nimport {b} from '../lib/b';\nexport {c} from './c';\nimport 'react';\n";
        let extracted = imports_of(src, "typescript");
        let mods: Vec<&str> = extracted.iter().map(|i| i.module.as_str()).collect();
        assert!(mods.contains(&"./a"), "{mods:?}");
        assert!(mods.contains(&"../lib/b"), "{mods:?}");
        assert!(mods.contains(&"./c"), "re-export source: {mods:?}");
        assert!(mods.contains(&"react"), "{mods:?}");
    }

    #[test]
    fn extracts_dart_imports_and_exports() {
        let src = "import 'dart:async';\n\
                   import 'package:args/args.dart';\n\
                   import 'src/parser.dart' as p;\n\
                   export 'src/arg_parser.dart' show ArgParser;\n";
        let extracted = imports_of(src, "dart");
        let mods: Vec<&str> = extracted.iter().map(|i| i.module.as_str()).collect();
        assert!(mods.contains(&"dart:async"), "{mods:?}");
        assert!(mods.contains(&"package:args/args.dart"), "{mods:?}");
        assert!(
            mods.contains(&"src/parser.dart"),
            "import with alias: {mods:?}"
        );
        assert!(
            mods.contains(&"src/arg_parser.dart"),
            "export source: {mods:?}"
        );
    }

    // --- resolution ---

    fn resolver(root: &Path, files: &[&str]) -> Resolver {
        let paths: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
        Resolver::new(root, &paths)
    }

    /// Real source declarations feed the same extraction/scoping/context path
    /// as a project index. The source strings form a complete Rust crate;
    /// unrelated same-named files deliberately remain in the project file set.
    fn rust_fixture(
        files: &[(&str, &str)],
    ) -> (Resolver, HashMap<PathBuf, Vec<RawImport>>, RustCtx) {
        let root = Path::new("/rust-fixture");
        let paths: Vec<PathBuf> = files.iter().map(|(file, _)| root.join(file)).collect();
        let resolver = Resolver::with_meta(root, &paths, None, None);
        let raw = files
            .iter()
            .map(|(file, src)| {
                (
                    root.join(file),
                    clew_core::rustscope::scope_rust_imports(src, imports_of(src, "rust")),
                )
            })
            .collect();
        let ctx = RustCtx::new(&raw, &lang_of);
        (resolver, raw, ctx)
    }

    #[test]
    fn nested_main_and_lib_modules_resolve_their_own_children_before_siblings() {
        let (r, raw, ctx) = rust_fixture(&[
            ("src/lib.rs", "pub mod commands;"),
            (
                "src/commands.rs",
                "pub mod main; pub mod lib; pub struct Parent;",
            ),
            (
                "src/commands/main.rs",
                "mod inner; pub use self::inner::Main; use super::Parent;",
            ),
            ("src/commands/main/inner.rs", "pub struct Main;"),
            (
                "src/commands/lib.rs",
                "mod inner; pub use self::inner::Lib; use super::Parent;",
            ),
            ("src/commands/lib/inner.rs", "pub struct Lib;"),
            ("src/commands/inner.rs", "pub struct Decoy;"),
        ]);
        for stem in ["main", "lib"] {
            let from = r.root.join(format!("src/commands/{stem}.rs"));
            let child = r.root.join(format!("src/commands/{stem}/inner.rs"));
            let targets: Vec<Target> = raw[&from]
                .iter()
                .map(|import| r.resolve_with(import, &from, "rust", &ctx, &Declared::default()))
                .collect();
            assert_eq!(
                targets,
                [
                    Target::Internal(child.clone()),
                    Target::Internal(child),
                    Target::Internal(r.root.join("src/commands.rs")),
                ]
            );
        }
    }

    #[test]
    fn known_crate_roots_keep_sibling_modules_regardless_of_filename() {
        for root_file in [
            "src/main.rs",
            "src/lib.rs",
            "src/bin/tool/main.rs",
            "src/bin/tool.rs",
            "examples/demo.rs",
            "tests/it.rs",
            "benches/perf.rs",
        ] {
            let parent = Path::new(root_file).parent().unwrap();
            let child_file = parent.join("child/mod.rs").to_string_lossy().into_owned();
            let stem = Path::new(root_file).file_stem().unwrap();
            let decoy_file = parent
                .join(stem)
                .join("child.rs")
                .to_string_lossy()
                .into_owned();
            let (r, raw, ctx) = rust_fixture(&[
                (
                    "Cargo.toml",
                    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"",
                ),
                (
                    root_file,
                    "mod child; use self::child::Item; use crate::child::Item as Alias; fn main() {}",
                ),
                (&child_file, "pub struct Item;"),
                (&decoy_file, "pub struct Decoy;"),
            ]);
            let from = r.root.join(root_file);
            for import in &raw[&from] {
                assert_eq!(
                    r.resolve_with(import, &from, "rust", &ctx, &Declared::default()),
                    Target::Internal(r.root.join(&child_file)),
                    "{root_file}: {import:?}"
                );
            }
        }
    }

    #[test]
    fn unknown_custom_main_and_lib_targets_retain_existing_sibling_child_lookup() {
        // Manifest-declared target roots are not currently inferred, but a
        // nonstandard main/lib with no ordinary-module child keeps the lookup
        // supported before the distinction between roots and modules.
        for stem in ["main", "lib"] {
            let root_file = format!("custom/{stem}.rs");
            let manifest = format!(
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n{}\npath = \"{root_file}\"",
                if stem == "main" {
                    "[[bin]]\nname = \"fixture\""
                } else {
                    "[lib]"
                },
            );
            let (r, raw, ctx) = rust_fixture(&[
                ("Cargo.toml", &manifest),
                (
                    &root_file,
                    "mod child; use self::child::Item; pub struct RootItem; fn main() {}",
                ),
                ("custom/child.rs", "use super::RootItem; pub struct Item;"),
            ]);
            let from = r.root.join(&root_file);
            for import in &raw[&from] {
                assert_eq!(
                    r.resolve_with(import, &from, "rust", &ctx, &Declared::default()),
                    Target::Internal(r.root.join("custom/child.rs")),
                    "{root_file}: {import:?}"
                );
            }
            let child = r.root.join("custom/child.rs");
            assert_eq!(
                r.resolve_with(&raw[&child][0], &child, "rust", &ctx, &Declared::default()),
                Target::Internal(from),
                "custom child still resolves its parent's items"
            );
        }
    }

    #[test]
    fn rust_raw_identifiers_resolve_module_files_reexports_and_item_keys() {
        let (r, raw, ctx) = rust_fixture(&[
            (
                "src/lib.rs",
                "pub mod r#type; pub mod user; pub use r#type::r#match;",
            ),
            ("src/type.rs", "pub struct r#match;"),
            (
                "src/user.rs",
                "use crate::r#type::r#match; use crate::r#match as Alias;",
            ),
        ]);
        for file in ["src/lib.rs", "src/user.rs"] {
            let from = r.root.join(file);
            for import in raw[&from].iter().filter(|import| import.module != "user") {
                assert_eq!(
                    r.resolve_with(import, &from, "rust", &ctx, &Declared::default()),
                    Target::Internal(r.root.join("src/type.rs")),
                    "{file}: {import:?}"
                );
                assert!(import.module.contains("r#"), "source spelling is preserved");
            }
        }
        assert_eq!(name_key("r#match"), name_key("match"));
        assert_ne!(name_key("r#match"), name_key("rmatch"));
    }

    #[test]
    fn same_line_inline_modules_resolve_to_their_separate_external_children() {
        let (r, raw, ctx) = rust_fixture(&[
            (
                "src/lib.rs",
                "pub mod a { mod inner; pub use self::inner::A; } pub mod b { mod inner; pub use self::inner::B; }",
            ),
            ("src/a/inner.rs", "pub struct A;"),
            ("src/b/inner.rs", "pub struct B;"),
        ]);
        let from = r.root.join("src/lib.rs");
        let targets: Vec<Target> = raw[&from]
            .iter()
            .map(|import| r.resolve_with(import, &from, "rust", &ctx, &Declared::default()))
            .collect();
        assert_eq!(
            targets,
            [
                Target::Internal(r.root.join("src/a/inner.rs")),
                Target::Internal(r.root.join("src/a/inner.rs")),
                Target::Internal(r.root.join("src/b/inner.rs")),
                Target::Internal(r.root.join("src/b/inner.rs")),
            ]
        );
    }

    /// Each Cargo target is its own crate: `src/bin/x.rs` and `examples/y.rs`
    /// resolve `crate::` against their own module directory, not the
    /// package's `src/` (which used to silently wire them to the lib's
    /// same-named modules).
    #[test]
    fn cargo_targets_each_get_their_own_crate_root() {
        let root = Path::new("/pkg");
        let r = resolver(
            root,
            &[
                "Cargo.toml",
                "src/lib.rs",
                "src/helper.rs",
                "src/bin/tool.rs",
                "src/bin/helper/mod.rs",
                "src/bin/tool/helper.rs",
                "examples/demo.rs",
                "examples/helper/mod.rs",
                "examples/demo/helper.rs",
            ],
        );

        // The lib resolves against src/.
        assert_eq!(
            r.resolve(&ri("crate::helper"), &root.join("src/lib.rs"), "rust"),
            Target::Internal(root.join("src/helper.rs"))
        );
        // A flat binary root's modules live beside its file, even though an
        // ordinary module named tool.rs would have children under tool/.
        assert_eq!(
            r.resolve(&ri("crate::helper"), &root.join("src/bin/tool.rs"), "rust"),
            Target::Internal(root.join("src/bin/helper/mod.rs"))
        );
        // …and so does a file inside that target's module tree.
        assert_eq!(
            r.resolve(
                &ri("crate::helper"),
                &root.join("src/bin/helper/mod.rs"),
                "rust"
            ),
            Target::Internal(root.join("src/bin/helper/mod.rs"))
        );
        // An example is a target too.
        assert_eq!(
            r.resolve(&ri("crate::helper"), &root.join("examples/demo.rs"), "rust"),
            Target::Internal(root.join("examples/helper/mod.rs"))
        );
    }

    /// Directory NAMES alone don't make a target: `src/tests/` is an ordinary
    /// module directory (Cargo's `tests/` lives at the package root), and a
    /// `tests/` with no Cargo.toml beside it is just a folder. Their files
    /// keep resolving against the enclosing crate.
    #[test]
    fn target_dirs_only_count_at_cargo_locations() {
        let root = Path::new("/pkg");
        let r = resolver(
            root,
            &[
                "Cargo.toml",
                "src/lib.rs",
                "src/helper.rs",
                "src/tests/helper.rs",
                "src/tests/cases.rs",
            ],
        );
        // A file in `src/tests/` belongs to the lib crate: `crate::helper`
        // from there is the lib's `src/helper.rs`, not a sibling.
        assert_eq!(
            r.resolve(
                &ri("crate::helper"),
                &root.join("src/tests/cases.rs"),
                "rust"
            ),
            Target::Internal(root.join("src/helper.rs"))
        );

        // The location predicate itself: target dirs count only where Cargo
        // looks for them.
        let files: HashSet<PathBuf> = ["/pkg/Cargo.toml"].iter().map(PathBuf::from).collect();
        let is_root = |p: &str| is_rust_crate_root(Path::new(p), &files);
        assert!(is_root("/pkg/tests/it.rs"), "package-root tests/");
        assert!(
            is_root("/pkg/src/bin/tool.rs"),
            "bin under the package src/"
        );
        assert!(
            !is_root("/pkg/src/tests/helper.rs"),
            "src/tests is a module"
        );
        assert!(!is_root("/pkg/deep/tests/it.rs"), "no Cargo.toml beside it");
        assert!(!is_root("/pkg/bin/tool.rs"), "bin outside src/ is a folder");
        // `main.rs`/`lib.rs` root a crate only where Cargo puts them; an
        // ordinary nested module by that name must keep resolving against
        // its enclosing crate.
        assert!(is_root("/pkg/src/main.rs"), "the default binary");
        assert!(is_root("/pkg/src/lib.rs"), "the library root");
        assert!(is_root("/pkg/src/bin/tool/main.rs"), "a dir-shaped bin");
        assert!(is_root("/pkg/tests/it/main.rs"), "a dir-shaped test");
        assert!(
            !is_root("/pkg/anywhere/main.rs"),
            "a nested module named main.rs is not a crate root"
        );
        assert!(
            !is_root("/pkg/src/commands/main.rs"),
            "a nested module named main.rs is not a crate root"
        );
        // `main` is a valid target name for the flat target directories too.
        assert!(is_root("/pkg/src/bin/main.rs"), "a bin named main");
        assert!(is_root("/pkg/tests/main.rs"), "a test named main");
        assert!(is_root("/pkg/examples/main.rs"), "an example named main");
        assert!(!is_root("/pkg/bin/main.rs"), "bin outside src/ is a folder");
    }

    /// In a workspace, `crate::` resolves against the importing file's own
    /// member crate — never a single project-wide root, which either left
    /// member imports unresolved or wired them to a same-named module of the
    /// root crate.
    #[test]
    fn workspace_crate_imports_resolve_per_member() {
        let root = Path::new("/ws");
        let r = resolver(
            root,
            &[
                "src/main.rs",
                "src/app.rs",
                "crates/core/src/lib.rs",
                "crates/core/src/store.rs",
                "crates/server/src/main.rs",
                "crates/server/src/agent.rs",
            ],
        );

        // A member's `crate::` stays inside the member.
        assert_eq!(
            r.resolve(
                &ri("crate::store::Thing"),
                &root.join("crates/core/src/lib.rs"),
                "rust"
            ),
            Target::Internal(root.join("crates/core/src/store.rs"))
        );
        assert_eq!(
            r.resolve(
                &ri("crate::agent"),
                &root.join("crates/server/src/main.rs"),
                "rust"
            ),
            Target::Internal(root.join("crates/server/src/agent.rs"))
        );
        // The root binary's `crate::` resolves against the root crate.
        assert_eq!(
            r.resolve(&ri("crate::app"), &root.join("src/main.rs"), "rust"),
            Target::Internal(root.join("src/app.rs"))
        );
        // A member does NOT reach the root crate's modules via `crate::` —
        // an unknown segment falls back to "an item in the member's own crate
        // root", never to the workspace root's same-named module.
        assert_eq!(
            r.resolve(
                &ri("crate::app"),
                &root.join("crates/core/src/lib.rs"),
                "rust"
            ),
            Target::Internal(root.join("crates/core/src/lib.rs"))
        );
    }

    /// A workspace's crates `use` one another by their libraries' names, as
    /// their manifests give them: `use acme_core::store::Thing` from the
    /// server crate is the core crate's `store.rs`, not an external crate —
    /// and a library at a path of its own (`[lib] path`) or a binary named
    /// in `[[bin]]` is a crate root, whose `crate::` reads beside it. A name
    /// no package has stays external.
    #[test]
    fn a_workspaces_crates_resolve_one_anothers_uses() {
        let root = Path::new("/ws");
        let files: Vec<PathBuf> = [
            "crates/core/Cargo.toml",
            "crates/core/src/lib.rs",
            "crates/core/src/store.rs",
            "crates/core/src/store/disk.rs",
            "crates/server/Cargo.toml",
            "crates/server/src/main.rs",
            "crates/proto/Cargo.toml",
            "crates/proto/wire/proto.rs",
            "crates/proto/wire/frame.rs",
            "crates/proto/tools/gen.rs",
            "crates/proto/tools/opts.rs",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let package = |name: &str, lib: &str, targets: &[&str]| clew_protocol::CargoTargets {
            lib_name: name.into(),
            lib_path: lib.into(),
            target_paths: targets.iter().map(|t| t.to_string()).collect(),
        };
        let manifests = clew_protocol::SourceManifests {
            cargo: vec![
                (
                    "crates/core/Cargo.toml".into(),
                    package("acme_core", "src/lib.rs", &[]),
                ),
                (
                    "crates/server/Cargo.toml".into(),
                    package("acme_server", "src/lib.rs", &[]),
                ),
                (
                    "crates/proto/Cargo.toml".into(),
                    package(
                        "proto",
                        "wire/proto.rs",
                        &["tools/gen.rs", "../../outside.rs"],
                    ),
                ),
            ],
            ..Default::default()
        };
        let r = Resolver::with_meta(root, &files, None, None).with_manifests(&manifests);
        let server = root.join("crates/server/src/main.rs");
        let at = |spec: &str, from: &Path| r.resolve(&ri(spec), from, "rust");
        assert_eq!(
            at("acme_core::store::Thing", &server),
            Target::Internal(root.join("crates/core/src/store.rs"))
        );
        assert_eq!(
            at("acme_core::store::disk::open", &server),
            Target::Internal(root.join("crates/core/src/store/disk.rs"))
        );
        assert_eq!(
            at("acme_core", &server),
            Target::Internal(root.join("crates/core/src/lib.rs"))
        );
        // An item of the library's root itself.
        assert_eq!(
            at("acme_core::Config", &server),
            Target::Internal(root.join("crates/core/src/lib.rs"))
        );
        // `[lib] path`: the library is where its manifest says, its
        // modules beside it.
        assert_eq!(
            at("proto::frame::Frame", &server),
            Target::Internal(root.join("crates/proto/wire/frame.rs"))
        );
        assert_eq!(
            at(
                "crate::frame::Frame",
                &root.join("crates/proto/wire/proto.rs")
            ),
            Target::Internal(root.join("crates/proto/wire/frame.rs"))
        );
        // `[[bin]] path`: a crate root, its `mod`s beside it.
        assert_eq!(
            at("crate::opts", &root.join("crates/proto/tools/gen.rs")),
            Target::Internal(root.join("crates/proto/tools/opts.rs"))
        );
        // A crate the project does not hold, and a server with no library.
        assert_eq!(
            at("serde::Serialize", &server),
            Target::External("serde".into())
        );
        assert_eq!(
            at("acme_server::run", &root.join("crates/core/src/lib.rs")),
            Target::External("acme_server".into())
        );
    }

    /// Off disk: a local project's `Cargo.toml`s and `pyproject.toml` are
    /// read when its resolver is made, and what they say is used.
    #[test]
    fn a_local_projects_cargo_and_pyproject_manifests_are_read() {
        let dir = clew_core::testutil::TempDir::new("cargo-pyproject");
        let write = |rel: &str, text: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(
            "crates/my-core/Cargo.toml",
            "[package]\nname = \"my-core\"\nversion = \"0.1.0\"\n",
        );
        write("crates/my-core/src/lib.rs", "pub mod store;\n");
        write("crates/my-core/src/store.rs", "");
        write("crates/app/src/main.rs", "use my_core::store;\n");
        write(
            "pyproject.toml",
            "[tool.maturin]\npython-source = \"python\"\n",
        );
        write("python/acme/__init__.py", "");
        write("python/acme/_native.pyi", "");
        write("python/acme/cli.py", "from acme import _native\n");
        let files: Vec<PathBuf> = [
            "Cargo.toml",
            "crates/my-core/Cargo.toml",
            "crates/my-core/src/lib.rs",
            "crates/my-core/src/store.rs",
            "crates/app/src/main.rs",
            "pyproject.toml",
            "python/acme/__init__.py",
            "python/acme/_native.pyi",
            "python/acme/cli.py",
        ]
        .iter()
        .map(|f| dir.join(f))
        .collect();
        let r = Resolver::new(&dir, &files);
        assert_eq!(
            r.resolve(
                &ri("my_core::store"),
                &dir.join("crates/app/src/main.rs"),
                "rust"
            ),
            Target::Internal(dir.join("crates/my-core/src/store.rs"))
        );
        // maturin's `python-source`: `acme` is the project's, and its
        // compiled module is read by its stub.
        assert_eq!(
            r.resolve(
                &ri("acme:_native"),
                &dir.join("tests/test_cli.py"),
                "python"
            ),
            Target::Internal(dir.join("python/acme/_native.pyi"))
        );
    }

    /// A Python module is its source where it has one, its stub (`.pyi`)
    /// where it has only that; and a source root a `pyproject.toml` names
    /// (`lib/` here) is looked in after the project's root, so a test's
    /// `import billing` is the project's own module, not a package's.
    #[test]
    fn python_resolves_declared_roots_and_stubs() {
        let root = Path::new("/py");
        let files: Vec<PathBuf> = [
            "pyproject.toml",
            "lib/billing/__init__.py",
            "lib/billing/rates.py",
            "lib/billing/rates.pyi",
            "lib/billing/_speedups.pyi",
            "lib/typed/__init__.pyi",
            "tests/test_billing.py",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let manifests = clew_protocol::SourceManifests {
            python: vec![(
                "pyproject.toml".into(),
                vec!["lib".into(), "missing".into(), "../elsewhere".into()],
            )],
            ..Default::default()
        };
        let r = Resolver::with_meta(root, &files, None, None).with_manifests(&manifests);
        assert_eq!(
            r.python_roots,
            [root.join("lib")],
            "only roots that exist, inside"
        );
        let test = root.join("tests/test_billing.py");
        let at = |spec: &str| r.resolve(&ri(spec), &test, "python");
        assert_eq!(
            at("billing"),
            Target::Internal(root.join("lib/billing/__init__.py"))
        );
        // The source, not its stub, where both are.
        assert_eq!(
            at("billing.rates"),
            Target::Internal(root.join("lib/billing/rates.py"))
        );
        assert_eq!(
            at("billing:_speedups"),
            Target::Internal(root.join("lib/billing/_speedups.pyi"))
        );
        assert_eq!(
            at("typed"),
            Target::Internal(root.join("lib/typed/__init__.pyi"))
        );
        assert_eq!(at("requests"), Target::External("requests".into()));
    }

    /// `#[path = "…"] mod x;` is the file it names: from the declaring
    /// file's folder, or inside an inline module from that module's folder.
    #[test]
    fn a_rust_module_with_a_path_attribute_is_the_file_it_names() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "src/lib.rs",
                "src/sys/windows.rs",
                "src/net.rs",
                "src/net/inner/x.rs",
                "src/imp.rs",
            ],
        );
        let decl = |module: &str| RawImport {
            module: module.into(),
            line: 1,
            is_mod_decl: true,
        };
        let at = |module: &str, from: &str| r.resolve(&decl(module), &root.join(from), "rust");
        assert_eq!(
            at("os=sys/windows.rs", "src/lib.rs"),
            Target::Internal(root.join("src/sys/windows.rs"))
        );
        // From a module file too, its own folder: `src/`, not `src/net/`.
        assert_eq!(
            at("imp=imp.rs", "src/net.rs"),
            Target::Internal(root.join("src/imp.rs"))
        );
        assert_eq!(
            at("inner::x=x.rs", "src/net.rs"),
            Target::Internal(root.join("src/net/inner/x.rs"))
        );
        assert_eq!(
            at("gone=nowhere.rs", "src/lib.rs"),
            Target::Unresolved("gone=nowhere.rs".into())
        );
    }

    #[test]
    fn resolves_rust_mod_and_use_paths() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "src/main.rs",
                "src/beta.rs",
                "src/lsp/mod.rs",
                "src/lsp/client.rs",
            ],
        );

        // `mod beta;` in main.rs → src/beta.rs
        let mod_decl = RawImport {
            module: "beta".into(),
            line: 1,
            is_mod_decl: true,
        };
        assert_eq!(
            r.resolve(&mod_decl, &root.join("src/main.rs"), "rust"),
            Target::Internal(root.join("src/beta.rs"))
        );

        // `use crate::lsp::client::CallItem;` → drops the item, resolves the module file.
        assert_eq!(
            r.resolve(
                &ri("crate::lsp::client::CallItem"),
                &root.join("src/main.rs"),
                "rust"
            ),
            Target::Internal(root.join("src/lsp/client.rs"))
        );

        // `use crate::lsp::X` → the module dir's mod.rs.
        assert_eq!(
            r.resolve(
                &ri("crate::lsp::Something"),
                &root.join("src/main.rs"),
                "rust"
            ),
            Target::Internal(root.join("src/lsp/mod.rs"))
        );

        // External crate.
        assert_eq!(
            r.resolve(&ri("serde::Deserialize"), &root.join("src/main.rs"), "rust"),
            Target::External("serde".into())
        );
        // Std is external too.
        assert_eq!(
            r.resolve(
                &ri("std::collections::HashMap"),
                &root.join("src/main.rs"),
                "rust"
            ),
            Target::External("std".into())
        );
    }

    #[test]
    fn resolves_rust_super_relative() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "src/main.rs",
                "src/lsp/mod.rs",
                "src/lsp/client.rs",
                "src/lsp/config.rs",
            ],
        );
        // From client.rs (plain file), `use super::config::X` → sibling config.rs.
        assert_eq!(
            r.resolve(
                &ri("super::config::Setting"),
                &root.join("src/lsp/client.rs"),
                "rust"
            ),
            Target::Internal(root.join("src/lsp/config.rs"))
        );

        // From lsp/mod.rs, `super` is the CRATE ROOT's modules, so `super::app`
        // must resolve to src/app.rs, not src/lsp/app.rs.
        let r2 = resolver(root, &["src/main.rs", "src/lsp/mod.rs", "src/app.rs"]);
        assert_eq!(
            r2.resolve(
                &ri("super::app::Thing"),
                &root.join("src/lsp/mod.rs"),
                "rust"
            ),
            Target::Internal(root.join("src/app.rs"))
        );
    }

    /// A Lean import is the file at its module's path under a source root
    /// holding the importer: the project's own root, a test tree's own copy
    /// before it (the deeper root), never a file whose root does not hold the
    /// importer. A module no file can be is its package's: the toolchain's or
    /// a dependency's.
    #[test]
    fn a_lean_import_resolves_under_the_importers_source_root() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "Geo/Point.lean",
                "Geo/Shape.lean",
                "test/Geo/Point.lean",
                "test/Run.lean",
                "lib/Extra/Thing.lean",
                "main.lean",
            ],
        );
        let at = |module: &str, from: &str| r.resolve(&ri(module), &root.join(from), "lean");
        assert_eq!(
            at("Geo.Point", "Geo/Shape.lean"),
            Target::Internal(root.join("Geo/Point.lean"))
        );
        assert_eq!(
            at("Geo.Point", "main.lean"),
            Target::Internal(root.join("Geo/Point.lean"))
        );
        assert_eq!(
            at("Geo.Point", "test/Run.lean"),
            Target::Internal(root.join("test/Geo/Point.lean"))
        );
        assert_eq!(
            at("Extra.Thing", "main.lean"),
            Target::Unresolved("Extra.Thing".into())
        );
        assert_eq!(
            at("Mathlib.Order.Basic", "main.lean"),
            Target::External("Mathlib".into())
        );
        assert_eq!(at("Init", "main.lean"), Target::External("Init".into()));
    }

    /// With lakefiles, a Lean module is found under a Lake package's folder
    /// or a `srcDir` it names: a library in `lib/` imported from the root's
    /// `Main.lean`, a sibling package; never a project folder that only
    /// looks like the toolchain's module (`MyLib/Lean/Meta.lean` is no
    /// `Lean.Meta`).
    #[test]
    fn a_lean_import_resolves_where_the_lakefiles_say() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "lakefile.lean",
                "Main.lean",
                "lib/Extra/Thing.lean",
                "MyLib/Foo.lean",
                "MyLib/Lean/Meta.lean",
                "sib/lakefile.lean",
                "sib/Sib/Core.lean",
            ],
        )
        .with_manifests(&clew_protocol::SourceManifests {
            lean: vec![
                ("lakefile.lean".into(), vec!["lib".into()]),
                ("sib/lakefile.lean".into(), Vec::new()),
            ],
            ..Default::default()
        });
        let at = |module: &str, from: &str| r.resolve(&ri(module), &root.join(from), "lean");
        assert_eq!(
            at("Extra.Thing", "Main.lean"),
            Target::Internal(root.join("lib/Extra/Thing.lean"))
        );
        assert_eq!(
            at("MyLib.Lean.Meta", "MyLib/Foo.lean"),
            Target::Internal(root.join("MyLib/Lean/Meta.lean"))
        );
        assert_eq!(
            at("Lean.Meta", "MyLib/Foo.lean"),
            Target::External("Lean".into())
        );
        assert_eq!(
            at("Sib.Core", "Main.lean"),
            Target::Internal(root.join("sib/Sib/Core.lean"))
        );
    }

    /// PHP names resolve by the PSR-4 map composer.json gives (`autoload` and
    /// `autoload-dev`), a name under a mapped prefix that no file is stays
    /// unresolved, and any other is its vendor's; a `require` is a path from
    /// the importer's directory (`__DIR__`), else from the project root.
    #[test]
    fn php_names_resolve_by_psr4_and_requires_by_path() {
        let dir = clew_core::testutil::TempDir::new("php-psr4");
        std::fs::write(
            dir.join("composer.json"),
            r#"{"autoload": {"psr-4": {"App\\": "app/"}},
                "autoload-dev": {"psr-4": {"Tests\\": ["tests/"]}}}"#,
        )
        .unwrap();
        let files: Vec<PathBuf> = [
            "composer.json",
            "app/Models/User.php",
            "app/Http/Controller.php",
            "lib/helpers.php",
            "tests/UserTest.php",
            "bootstrap.php",
        ]
        .iter()
        .map(|f| dir.join(f))
        .collect();
        let r = Resolver::new(&dir, &files);
        let from = dir.join("app/Http/Controller.php");
        let at = |spec: &str| r.resolve(&ri(spec), &from, "php");
        assert_eq!(
            at("App\\Models\\User"),
            Target::Internal(dir.join("app/Models/User.php"))
        );
        assert_eq!(
            at("Tests\\UserTest"),
            Target::Internal(dir.join("tests/UserTest.php"))
        );
        assert_eq!(
            at("App\\Models\\Missing"),
            Target::Unresolved("App\\Models\\Missing".into())
        );
        assert_eq!(
            at("Symfony\\Component\\Console\\Command"),
            Target::External("Symfony".into())
        );
        assert_eq!(
            at("__DIR__/../../lib/helpers.php"),
            Target::Internal(dir.join("lib/helpers.php"))
        );
        assert_eq!(
            at("bootstrap.php"),
            Target::Internal(dir.join("bootstrap.php"))
        );
        assert_eq!(at("nowhere.php"), Target::Unresolved("nowhere.php".into()));
        // A remote project's resolver resolves the same with the maps the
        // server read; without them, a class is only a path from the root.
        let rels: Vec<String> = files
            .iter()
            .map(|f| {
                f.strip_prefix(&*dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let remote = Resolver::with_meta(&dir, &files, None, None);
        let class = |r: &Resolver| r.resolve(&ri("App\\Models\\User"), &from, "php");
        assert_ne!(
            class(&remote),
            Target::Internal(dir.join("app/Models/User.php"))
        );
        let remote = remote.with_manifests(&clew_core::imports::source_manifests(&dir, &rels));
        assert_eq!(
            class(&remote),
            Target::Internal(dir.join("app/Models/User.php"))
        );
        assert_eq!(
            remote.resolve(&ri("Tests\\UserTest"), &from, "php"),
            Target::Internal(dir.join("tests/UserTest.php"))
        );
    }

    /// A Ruby `require_relative` is a file beside the importer; a `require`
    /// is found on the load path — the importer's own gem's `lib`, the
    /// project's `lib/`, `spec/` for RSpec's helper — else it is a gem.
    #[test]
    fn ruby_requires_resolve_on_the_load_path() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "lib/acme.rb",
                "lib/acme/widget.rb",
                "spec/spec_helper.rb",
                "spec/acme_spec.rb",
                "gems/tool/lib/tool/cli.rb",
                "gems/tool/lib/tool/version.rb",
                "Rakefile",
                "tasks/setup.rb",
            ],
        );
        let at = |spec: &str, from: &str| r.resolve(&ri(spec), &root.join(from), "ruby");
        assert_eq!(
            at("./acme/widget", "lib/acme.rb"),
            Target::Internal(root.join("lib/acme/widget.rb"))
        );
        assert_eq!(
            at("acme/widget", "lib/acme.rb"),
            Target::Internal(root.join("lib/acme/widget.rb"))
        );
        assert_eq!(
            at("spec_helper", "spec/acme_spec.rb"),
            Target::Internal(root.join("spec/spec_helper.rb"))
        );
        assert_eq!(
            at("acme", "spec/acme_spec.rb"),
            Target::Internal(root.join("lib/acme.rb"))
        );
        assert_eq!(
            at("tool/version", "gems/tool/lib/tool/cli.rb"),
            Target::Internal(root.join("gems/tool/lib/tool/version.rb"))
        );
        assert_eq!(
            at("./tasks/setup.rb", "Rakefile"),
            Target::Internal(root.join("tasks/setup.rb"))
        );
        assert_eq!(
            at("active_support/all", "lib/acme.rb"),
            Target::External("active_support".into())
        );
        assert_eq!(
            at("./missing", "lib/acme.rb"),
            Target::Unresolved("./missing".into())
        );
    }

    /// An Erlang header is found beside the importer or in its
    /// application's `include/` (else the project's); an `-include_lib` in
    /// the application it names, wherever the project keeps it — by its
    /// `.app.src`, or by its directory's name — else it is that
    /// application's, outside. A behaviour or an import is the module's
    /// file wherever it is, else a library's.
    #[test]
    fn erlang_headers_and_modules_resolve_across_applications() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "apps/shop/src/shop.app.src",
                "apps/shop/src/cart.erl",
                "apps/shop/include/records.hrl",
                "apps/core/src/my_behaviour.erl",
                "apps/core/include/core.hrl",
                "apps/payments-service/src/payments.app.src",
                "apps/payments-service/include/money.hrl",
                "deps/cowboy/include/cowboy.hrl",
                "include/global.hrl",
            ],
        );
        let from = root.join("apps/shop/src/cart.erl");
        let at = |spec: &str| r.resolve(&ri(spec), &from, "erlang");
        let internal = |f: &str| Target::Internal(root.join(f));
        assert_eq!(at("records.hrl"), internal("apps/shop/include/records.hrl"));
        assert_eq!(at("global.hrl"), internal("include/global.hrl"));
        assert_eq!(
            at("lib:core/include/core.hrl"),
            internal("apps/core/include/core.hrl")
        );
        assert_eq!(
            at("lib:payments/include/money.hrl"),
            internal("apps/payments-service/include/money.hrl")
        );
        assert_eq!(
            at("lib:cowboy/include/cowboy.hrl"),
            internal("deps/cowboy/include/cowboy.hrl")
        );
        assert_eq!(
            at("lib:kernel/include/file.hrl"),
            Target::External("kernel".into())
        );
        assert_eq!(
            at("my_behaviour"),
            internal("apps/core/src/my_behaviour.erl")
        );
        assert_eq!(at("gen_server"), Target::External("gen_server".into()));
        assert_eq!(at("missing.hrl"), Target::Unresolved("missing.hrl".into()));
    }

    /// A Haskell module is the file under a source root: the ones the
    /// package descriptions name (a `.cabal` file's `hs-source-dirs`, a
    /// `package.yaml`'s `source-dirs`), or a conventional one; a module no
    /// file is is a package's.
    #[test]
    fn haskell_modules_resolve_under_the_declared_source_dirs() {
        let dir = clew_core::testutil::TempDir::new("haskell-roots");
        std::fs::write(
            dir.join("acme.cabal"),
            "library\n  hs-source-dirs: lib-src\nexecutable acme\n  hs-source-dirs: app\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("pkgs/util")).unwrap();
        std::fs::write(dir.join("pkgs/util/package.yaml"), "source-dirs: source\n").unwrap();
        let names = [
            "acme.cabal",
            "lib-src/Acme/Types.hs",
            "app/Main.hs",
            "test/Acme/TypesSpec.hs",
            "pkgs/util/package.yaml",
            "pkgs/util/source/Util/Text.hs",
        ];
        let files: Vec<PathBuf> = names.iter().map(|f| dir.join(f)).collect();
        let r = Resolver::new(&dir, &files);
        let at = |module: &str, from: &str| r.resolve(&ri(module), &dir.join(from), "haskell");
        assert_eq!(
            at("Acme.Types", "app/Main.hs"),
            Target::Internal(dir.join("lib-src/Acme/Types.hs"))
        );
        assert_eq!(
            at("Acme.Types", "test/Acme/TypesSpec.hs"),
            Target::Internal(dir.join("lib-src/Acme/Types.hs"))
        );
        assert_eq!(
            at("Util.Text", "app/Main.hs"),
            Target::Internal(dir.join("pkgs/util/source/Util/Text.hs"))
        );
        assert_eq!(
            at("Data.Map", "app/Main.hs"),
            Target::External("Data.Map".into())
        );
        // Without the descriptions (a remote project's resolver before the
        // server's snapshot), only the conventional roots are known…
        let remote = Resolver::with_meta(&dir, &files, None, None);
        assert_eq!(
            remote.resolve(&ri("Acme.Types"), &dir.join("app/Main.hs"), "haskell"),
            Target::External("Acme.Types".into())
        );
        // …and with what the server read of them, it resolves as a local one.
        let sent = clew_core::imports::source_manifests(&dir, &names);
        assert_eq!(sent.haskell.len(), 2, "{sent:?}");
        let remote = remote.with_manifests(&sent);
        for (module, file) in [
            ("Acme.Types", "lib-src/Acme/Types.hs"),
            ("Util.Text", "pkgs/util/source/Util/Text.hs"),
        ] {
            assert_eq!(
                remote.resolve(&ri(module), &dir.join("app/Main.hs"), "haskell"),
                Target::Internal(dir.join(file)),
                "{module}"
            );
        }
    }

    /// A Zig `@import` of a path is the file beside the importer; a bare
    /// name is a package.
    /// A C or C++ `#include "x.h"` is the file beside the includer, then —
    /// as an `#include <x.h>` is — the project file whose path ends in the
    /// include's, the nearest of several; a header no project file is, is
    /// its library's.
    #[test]
    fn c_includes_resolve_beside_then_by_path_and_name_libraries() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "src/main.cpp",
                "src/cart.hpp",
                "src/detail/util.h",
                "include/shop/order.hpp",
                "include/shop/cart.hpp",
                "lib/a/config.h",
                "lib/b/config.h",
                "lib/b/b.c",
                "x.inc",
            ],
        );
        let at = |spec: &str, from: &str| r.resolve(&ri(spec), &root.join(from), "cpp");
        let file = |f: &str| Target::Internal(root.join(f));
        assert_eq!(at("cart.hpp", "src/main.cpp"), file("src/cart.hpp"));
        assert_eq!(at("./cart.hpp", "src/main.cpp"), file("src/cart.hpp"));
        assert_eq!(
            at("detail/util.h", "src/main.cpp"),
            file("src/detail/util.h")
        );
        assert_eq!(at("../x.inc", "src/main.cpp"), file("x.inc"));
        // Through the include path: by the path's end.
        assert_eq!(
            at("shop/order.hpp", "src/main.cpp"),
            file("include/shop/order.hpp")
        );
        assert_eq!(
            at("<shop/cart.hpp>", "src/main.cpp"),
            file("include/shop/cart.hpp")
        );
        // An angle include is never the file beside the includer first.
        assert_eq!(
            at("<cart.hpp>", "include/shop/order.hpp"),
            file("include/shop/cart.hpp")
        );
        // Of two, the one nearest the includer.
        assert_eq!(at("config.h", "lib/b/b.c"), file("lib/b/config.h"));
        // Libraries.
        for (spec, library) in [
            ("<vector>", "std"),
            ("<cstdio>", "std"),
            ("<bits/stdc++.h>", "std"),
            ("<stdio.h>", "libc"),
            ("<unistd.h>", "posix"),
            ("<sys/stat.h>", "posix"),
            ("<boost/asio.hpp>", "boost"),
            ("<zlib.h>", "zlib"),
            ("gtest/gtest.h", "gtest"),
        ] {
            assert_eq!(
                at(spec, "src/main.cpp"),
                Target::External(library.into()),
                "{spec}"
            );
        }
        assert_eq!(at("<>", "src/main.cpp"), Target::Unresolved("<>".into()));
    }

    #[test]
    fn zig_imports_resolve_files_and_leave_packages_external() {
        let root = Path::new("/p");
        let r = resolver(root, &["build.zig", "src/main.zig", "src/util/strings.zig"]);
        let at = |spec: &str| r.resolve(&ri(spec), &root.join("src/main.zig"), "zig");
        assert_eq!(
            at("util/strings.zig"),
            Target::Internal(root.join("src/util/strings.zig"))
        );
        assert_eq!(at("../build.zig"), Target::Internal(root.join("build.zig")));
        assert_eq!(at("std"), Target::External("std".into()));
        assert_eq!(at("missing.zig"), Target::Unresolved("missing.zig".into()));
        // Zig imports no file by an absolute path, even one in the project.
        assert_eq!(
            at("/p/build.zig"),
            Target::Unresolved("/p/build.zig".into())
        );
    }

    /// A name a project's `build.zig` gives one of its modules is that
    /// module's root file, for the files under that build.zig (a nested
    /// package's own first); a name it gives none, or a module out of the
    /// project, stays a package.
    #[test]
    fn zig_imports_of_a_module_the_build_names_resolve_to_its_file() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "build.zig",
                "src/main.zig",
                "src/greet.zig",
                "lib/util.zig",
                "pkg/build.zig",
                "pkg/src/main.zig",
                "pkg/src/greet.zig",
            ],
        )
        .with_manifests(&clew_protocol::SourceManifests {
            zig: vec![
                (
                    "build.zig".into(),
                    vec![
                        ("greet".into(), "src/greet.zig".into()),
                        ("util".into(), "lib/util.zig".into()),
                        ("gone".into(), "src/gone.zig".into()),
                    ],
                ),
                (
                    "pkg/build.zig".into(),
                    vec![("greet".into(), "src/greet.zig".into())],
                ),
            ],
            ..Default::default()
        });
        let at = |from: &str, spec: &str| r.resolve(&ri(spec), &root.join(from), "zig");
        assert_eq!(
            at("src/main.zig", "greet"),
            Target::Internal(root.join("src/greet.zig"))
        );
        assert_eq!(
            at("src/main.zig", "util"),
            Target::Internal(root.join("lib/util.zig"))
        );
        assert_eq!(
            at("pkg/src/main.zig", "greet"),
            Target::Internal(root.join("pkg/src/greet.zig"))
        );
        assert_eq!(
            at("pkg/src/main.zig", "util"),
            Target::External("util".into()),
            "a nested package sees its own build's names"
        );
        assert_eq!(at("src/main.zig", "gone"), Target::External("gone".into()));
        assert_eq!(at("src/main.zig", "std"), Target::External("std".into()));
        assert!(is_resolution_metadata(Path::new("pkg/build.zig")));
    }

    /// The manifests a local resolver reads for the whole project re-resolve
    /// it when they change.
    #[test]
    fn php_and_haskell_manifests_are_resolution_metadata() {
        for name in [
            "composer.json",
            "acme.cabal",
            "package.yaml",
            "go.mod",
            "Cargo.toml",
            "pyproject.toml",
            "package.json",
        ] {
            assert!(is_resolution_metadata(Path::new(name)), "{name}");
        }
        assert!(!is_resolution_metadata(Path::new(".cabal")));
        assert!(!is_resolution_metadata(Path::new("package-lock.json")));
    }

    #[test]
    fn go_module_prefix_needs_a_segment_boundary() {
        let dir = clew_core::testutil::TempDir::new("go-boundary");
        std::fs::write(dir.join("go.mod"), "module example.com/foo\n").unwrap();
        std::fs::create_dir_all(dir.join("bar")).unwrap();
        std::fs::write(dir.join("bar/x.go"), "package bar\n").unwrap();
        let r = Resolver::new(&dir, &[dir.join("bar/x.go")]);

        // A real sub-package resolves internally.
        assert_eq!(
            r.resolve(&ri("example.com/foo/bar"), &dir.join("main.go"), "go"),
            Target::Internal(dir.join("bar"))
        );
        // But `example.com/foobar` shares only a string prefix — it's external.
        assert_eq!(
            r.resolve(&ri("example.com/foobar/x"), &dir.join("main.go"), "go"),
            Target::External("example.com/foobar/x".into())
        );
    }

    #[test]
    fn fan_in_counts_distinct_importers_not_import_statements() {
        let root = Path::new("/proj");
        let r = resolver(root, &["src/lib.rs", "src/a.rs", "src/b.rs"]);
        let mut raw = HashMap::new();
        // b imports a TWICE (two use statements, same target file).
        raw.insert(
            root.join("src/b.rs"),
            vec![ri("crate::a::Foo"), ri("crate::a::Bar")],
        );
        raw.insert(root.join("src/a.rs"), vec![]);
        let g = ImportGraph::build(raw, &r, lang_of);
        assert_eq!(g.fan_in(&root.join("src/a.rs")), 1, "one importer, not two");
        assert_eq!(
            g.importers(&root.join("src/a.rs")),
            vec![root.join("src/b.rs")]
        );
        assert_eq!(g.internal_edge_count(), 1, "one distinct file→file edge");
    }

    #[test]
    fn tarjan_handles_a_deep_chain_without_stack_overflow() {
        // A long linear chain f0→f1→…→fN would overflow a recursive Tarjan.
        let root = Path::new("/proj");
        let n = 20_000;
        let files: Vec<String> = (0..n).map(|i| format!("src/f{i}.rs")).collect();
        let mut file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
        file_refs.push("src/lib.rs");
        let r = resolver(root, &file_refs);
        let mut raw = HashMap::new();
        for i in 0..n - 1 {
            raw.insert(
                root.join(format!("src/f{i}.rs")),
                vec![ri(&format!("crate::f{}::X", i + 1))],
            );
        }
        raw.insert(root.join(format!("src/f{}.rs", n - 1)), vec![]);
        let g = ImportGraph::build(raw, &r, lang_of);
        // A DAG chain has no cycles; the point is that this returns without abort.
        assert!(g.cycles().is_empty());
    }

    #[test]
    fn resolves_python_relative_and_absolute() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "pkg/__init__.py",
                "pkg/mod_a.py",
                "pkg/sub/__init__.py",
                "top.py",
            ],
        );

        // `from .mod_a import x` inside pkg/__init__.py → pkg/mod_a.py
        let rel = RawImport {
            module: ".mod_a".into(),
            line: 1,
            is_mod_decl: false,
        };
        assert_eq!(
            r.resolve(&rel, &root.join("pkg/__init__.py"), "python"),
            Target::Internal(root.join("pkg/mod_a.py"))
        );

        // `import top` → top.py at the project root.
        assert_eq!(
            r.resolve(&ri("top"), &root.join("pkg/mod_a.py"), "python"),
            Target::Internal(root.join("top.py"))
        );

        // `import numpy` → external.
        assert_eq!(
            r.resolve(&ri("numpy"), &root.join("top.py"), "python"),
            Target::External("numpy".into())
        );
    }

    /// A `src/` layout's module, imported from a test beside it (pytest's
    /// `pythonpath = src`, Pyright's own default): the project's file, not a
    /// package named `cart`.
    #[test]
    fn a_python_import_reaches_a_src_layout_module_from_tests() {
        let root = Path::new("/proj");
        let r = resolver(root, &["src/cart.py", "src/main.py", "tests/test_cart.py"]);
        assert_eq!(
            r.resolve(&ri("cart"), &root.join("tests/test_cart.py"), "python"),
            Target::Internal(root.join("src/cart.py"))
        );
        assert_eq!(
            r.resolve(&ri("cart"), &root.join("src/main.py"), "python"),
            Target::Internal(root.join("src/cart.py"))
        );
    }

    /// A package's module named like the standard library's (Flask's
    /// `flask/json/`, `flask/typing.py`) is never what `import json` or
    /// `import typing` reads, in the package or beside it: Python 3's
    /// imports are absolute, and a folder inside a package is not on
    /// `sys.path`. Nor is a package's module what a third-party name reads.
    #[test]
    fn a_python_package_module_never_shadows_an_absolute_import() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "src/flask/__init__.py",
                "src/flask/app.py",
                "src/flask/typing.py",
                "src/flask/json/__init__.py",
                "src/flask/json/provider.py",
                "src/flask/requests.py",
            ],
        );
        let json_init = root.join("src/flask/json/__init__.py");
        let app = root.join("src/flask/app.py");
        assert_eq!(
            r.resolve(&ri("json"), &json_init, "python"),
            Target::External("json".into())
        );
        assert_eq!(
            r.resolve(&ri("typing"), &app, "python"),
            Target::External("typing".into())
        );
        assert_eq!(
            r.resolve(&ri("requests"), &app, "python"),
            Target::External("requests".into())
        );
        // The package's own names still resolve, from inside and outside.
        assert_eq!(
            r.resolve(&ri("flask.typing"), &app, "python"),
            Target::Internal(root.join("src/flask/typing.py"))
        );
        assert_eq!(
            r.resolve(&ri("flask.json.provider"), &json_init, "python"),
            Target::Internal(root.join("src/flask/json/provider.py"))
        );
    }

    /// A script's own folder is the first place its imports are found, so a
    /// file named like the standard library's beside a script does shadow it,
    /// as it does when Python runs that script; elsewhere the standard
    /// library's name stays external.
    #[test]
    fn a_python_script_folder_is_searched_first() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "tools/run.py",
                "tools/random.py",
                "tools/helpers.py",
                "app/__init__.py",
                "app/main.py",
                "random.py",
            ],
        );
        assert_eq!(
            r.resolve(&ri("random"), &root.join("tools/run.py"), "python"),
            Target::Internal(root.join("tools/random.py"))
        );
        assert_eq!(
            r.resolve(&ri("helpers"), &root.join("tools/run.py"), "python"),
            Target::Internal(root.join("tools/helpers.py"))
        );
        // `app/main.py` runs from the root (its package's parent): the
        // root's `random.py` is what Python would find.
        assert_eq!(
            r.resolve(&ri("random"), &root.join("app/main.py"), "python"),
            Target::Internal(root.join("random.py"))
        );
        // A tool's sibling is not on the package's path.
        assert_eq!(
            r.resolve(&ri("helpers"), &root.join("app/main.py"), "python"),
            Target::External("helpers".into())
        );
    }

    /// A service's folder in a monorepo is a base for its own packages.
    #[test]
    fn a_python_monorepo_service_folder_holds_its_packages() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "services/api/app/__init__.py",
                "services/api/app/models.py",
                "services/api/app/routes/__init__.py",
                "services/api/app/routes/users.py",
            ],
        );
        assert_eq!(
            r.resolve(
                &ri("app.models"),
                &root.join("services/api/app/routes/users.py"),
                "python"
            ),
            Target::Internal(root.join("services/api/app/models.py"))
        );
    }

    #[test]
    fn the_python_stdlib_list_is_sorted_and_answers() {
        assert!(PY_STDLIB.windows(2).all(|w| w[0] < w[1]));
        for name in [
            "json",
            "typing",
            "os",
            "asyncio",
            "cProfile",
            "__future__",
            "distutils",
        ] {
            assert!(python_stdlib(name), "{name}");
        }
        for name in ["flask", "requests", "numpy", "_json"] {
            assert!(!python_stdlib(name), "{name}");
        }
    }

    /// `.`, `..` and a path ending in `/` name a directory, whose index is
    /// the target — not a file beside it of the directory's name.
    #[test]
    fn a_js_directory_import_takes_its_index_not_a_namesake_file() {
        let root = Path::new("/proj");
        let r = resolver(
            root,
            &[
                "src.ts",
                "src/index.ts",
                "src/a.ts",
                "src/a/index.ts",
                "src/a/b.ts",
                "src/util.ts",
                "src/util/index.ts",
            ],
        );
        let from = root.join("src/a/b.ts");
        let res = |spec: &str| r.resolve(&ri(spec), &from, "typescript");
        assert_eq!(res(".."), Target::Internal(root.join("src/index.ts")));
        assert_eq!(res("."), Target::Internal(root.join("src/a/index.ts")));
        assert_eq!(
            res("../util/"),
            Target::Internal(root.join("src/util/index.ts"))
        );
        // Without the slash, the file comes first, as it does for Node.
        assert_eq!(res("../util"), Target::Internal(root.join("src/util.ts")));
    }

    #[test]
    fn resolves_js_relative_with_extensions_and_index() {
        let root = Path::new("/proj");
        let r = resolver(root, &["src/app.ts", "src/util.ts", "src/widget/index.tsx"]);

        // './util' → util.ts (extension added)
        assert_eq!(
            r.resolve(&ri("./util"), &root.join("src/app.ts"), "typescript"),
            Target::Internal(root.join("src/util.ts"))
        );
        // './widget' → widget/index.tsx (index file)
        assert_eq!(
            r.resolve(&ri("./widget"), &root.join("src/app.ts"), "typescript"),
            Target::Internal(root.join("src/widget/index.tsx"))
        );
        // 'react' → external; '@scope/pkg' keeps the scope.
        assert_eq!(
            r.resolve(&ri("react"), &root.join("src/app.ts"), "typescript"),
            Target::External("react".into())
        );
        assert_eq!(
            r.resolve(
                &ri("@scope/pkg/sub"),
                &root.join("src/app.ts"),
                "typescript"
            ),
            Target::External("@scope/pkg".into())
        );
    }

    // --- graph ---

    fn lang_of(p: &Path) -> Option<&'static str> {
        crate::highlight::detect(p)
    }

    #[test]
    fn builds_graph_with_forward_and_reverse_edges() {
        let root = Path::new("/proj");
        let files = ["src/main.rs", "src/a.rs", "src/b.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        // main imports a and b; a imports b.
        raw.insert(
            root.join("src/main.rs"),
            vec![
                RawImport {
                    module: "a".into(),
                    line: 1,
                    is_mod_decl: true,
                },
                RawImport {
                    module: "b".into(),
                    line: 2,
                    is_mod_decl: true,
                },
            ],
        );
        raw.insert(root.join("src/a.rs"), vec![ri("crate::b::Thing")]);
        raw.insert(root.join("src/b.rs"), vec![]);

        let g = ImportGraph::build(raw, &r, lang_of);
        // b is imported by main and a.
        assert_eq!(
            g.importers(&root.join("src/b.rs")),
            vec![root.join("src/a.rs"), root.join("src/main.rs")]
        );
        // main imports two internal files.
        let internal: Vec<_> = g
            .imports(&root.join("src/main.rs"))
            .iter()
            .filter(|e| matches!(e.target, Target::Internal(_)))
            .collect();
        assert_eq!(internal.len(), 2);
    }

    #[test]
    fn set_file_updates_reverse_index() {
        let root = Path::new("/proj");
        let files = ["src/main.rs", "src/a.rs", "src/b.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        raw.insert(
            root.join("src/main.rs"),
            vec![RawImport {
                module: "a".into(),
                line: 1,
                is_mod_decl: true,
            }],
        );
        raw.insert(root.join("src/a.rs"), vec![]);
        raw.insert(root.join("src/b.rs"), vec![]);
        let mut g = ImportGraph::build(raw, &r, lang_of);
        assert_eq!(
            g.importers(&root.join("src/a.rs")),
            vec![root.join("src/main.rs")]
        );
        assert!(g.importers(&root.join("src/b.rs")).is_empty());

        // main now imports b instead of a.
        let mut batch = ImportBatch::default();
        batch.set(
            root.join("src/main.rs"),
            FileImports {
                raw: vec![RawImport {
                    module: "b".into(),
                    line: 1,
                    is_mod_decl: true,
                }],
                items: RustItems::new(),
            },
        );
        assert!(g.apply(&batch, &r, lang_of).changed);
        assert!(
            g.importers(&root.join("src/a.rs")).is_empty(),
            "a's importer was retracted"
        );
        assert_eq!(
            g.importers(&root.join("src/b.rs")),
            vec![root.join("src/main.rs")]
        );
    }

    #[test]
    fn detects_import_cycles() {
        let root = Path::new("/proj");
        let files = ["src/lib.rs", "src/a.rs", "src/b.rs", "src/c.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        // a → b → a is a cycle; c is standalone.
        raw.insert(root.join("src/a.rs"), vec![ri("crate::b::X")]);
        raw.insert(root.join("src/b.rs"), vec![ri("crate::a::Y")]);
        raw.insert(root.join("src/c.rs"), vec![]);
        let g = ImportGraph::build(raw, &r, lang_of);
        let cycles = g.cycles();
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert_eq!(
            cycles[0],
            vec![root.join("src/a.rs"), root.join("src/b.rs")]
        );
    }

    // --- tree ---

    #[test]
    fn tree_expands_imports_and_marks_cycles() {
        let root = Path::new("/proj");
        let files = ["src/lib.rs", "src/a.rs", "src/b.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        raw.insert(root.join("src/a.rs"), vec![ri("crate::b::X")]);
        raw.insert(root.join("src/b.rs"), vec![ri("crate::a::Y")]);
        let g = ImportGraph::build(raw, &r, lang_of);

        let mut tree = ImportTree::new(&g, root, root.join("src/a.rs"), Dir::Imports);
        // root a, expanded once, shows b.
        let vis = tree.visible();
        assert_eq!(vis.len(), 2);
        assert_eq!(tree.node(vis[1]).label, "b.rs");

        // Expanding b reveals a again — but it's on the ancestor path → cyclic leaf.
        let b_id = vis[1];
        tree.expand(b_id, &g, root);
        let vis = tree.visible();
        let a_again = *vis.last().unwrap();
        assert_eq!(tree.node(a_again).label, "a.rs");
        assert!(tree.node(a_again).cyclic);
    }

    #[test]
    fn tree_importers_direction() {
        let root = Path::new("/proj");
        let files = ["src/main.rs", "src/util.rs"];
        let r = resolver(root, &files);
        let mut raw = HashMap::new();
        raw.insert(
            root.join("src/main.rs"),
            vec![RawImport {
                module: "util".into(),
                line: 1,
                is_mod_decl: true,
            }],
        );
        raw.insert(root.join("src/util.rs"), vec![]);
        let g = ImportGraph::build(raw, &r, lang_of);

        // Who imports util? → main.
        let tree = ImportTree::new(&g, root, root.join("src/util.rs"), Dir::Importers);
        let vis = tree.visible();
        assert_eq!(vis.len(), 2);
        assert_eq!(tree.node(vis[1]).label, "main.rs");
    }
}

/// Rust module-tree semantics (E1-2) and JS/TS/Python/Dart resolution (E1-8).
#[cfg(test)]
mod resolution_tests {
    use super::*;

    fn ri(module: &str) -> RawImport {
        RawImport {
            module: module.into(),
            line: 1,
            is_mod_decl: false,
        }
    }

    fn lang_of(p: &Path) -> Option<&'static str> {
        crate::highlight::detect(p)
    }

    fn resolver(root: &Path, files: &[&str]) -> Resolver {
        let paths: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
        Resolver::with_meta(root, &paths, None, None)
    }

    /// Build the graph from real source text through the same pass the local
    /// index runs (`index::analyze_file`: one parse, inline-module scoping
    /// included) — resolving glob re-exports through the files' own symbols,
    /// as the app does ([`rust_item_keys`] in each file's [`FileImports`]),
    /// when `symbols` is set.
    fn graph_with(root: &Path, sources: &[(&str, &str)], symbols: bool) -> ImportGraph {
        let names: Vec<&str> = sources.iter().map(|(f, _)| *f).collect();
        let mut batch = ImportBatch::default();
        for (f, src) in sources {
            let lang = lang_of(Path::new(f)).expect("a known language");
            let abs = root.join(f);
            let facts = crate::index::analyze_file(&abs, f, src, lang);
            let items = if symbols {
                rust_item_keys(&abs, &facts.symbols)
            } else {
                RustItems::new()
            };
            batch.set(
                abs,
                FileImports {
                    raw: facts.imports,
                    items,
                },
            );
        }
        let mut g = ImportGraph::default();
        g.apply(&batch, &resolver(root, &names), lang_of);
        g
    }

    fn graph_from_sources(root: &Path, sources: &[(&str, &str)]) -> ImportGraph {
        graph_with(root, sources, true)
    }

    fn internal_uses(g: &ImportGraph, file: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = g
            .imports(file)
            .iter()
            .filter(|e| e.kind.is_dependency())
            .filter_map(|e| match &e.target {
                Target::Internal(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The kinds of every edge from `from` to `to`.
    fn kinds(g: &ImportGraph, from: &Path, to: &Path) -> Vec<EdgeKind> {
        g.imports(from)
            .iter()
            .filter(|e| e.target == Target::Internal(to.to_path_buf()))
            .map(|e| e.kind)
            .collect()
    }

    /// A child module is part of the module that declares it: using what
    /// that module defines (`use super::CompressionAlgo;`, `use
    /// crate::Error;`) is not a dependency a cycle is made of, while the
    /// module's own use of its child is. Two siblings that use each other
    /// still are one — salvo's shapes, both of them: its `compression`
    /// crate's root and children, and its core's `handler.rs` and
    /// `routing.rs` (`Handler` takes a `FlowCtrl`, the router holds
    /// handlers).
    #[test]
    fn a_child_using_its_enclosing_module_is_no_cycle_but_siblings_are() {
        let root = Path::new("/salvo");
        let sources: &[(&str, &str)] = &[
            (
                "src/lib.rs",
                "mod encoder;\nmod stream;\nmod handler;\nmod routing;\n\
                 pub use handler::Handler;\npub use routing::FlowCtrl;\n\
                 use encoder::Encoder;\nuse stream::EncodeStream;\n\
                 pub enum CompressionAlgo { Gzip }\npub struct Error;\n",
            ),
            (
                "src/encoder.rs",
                "use super::CompressionAlgo;\nuse crate::Error;\npub struct Encoder;\n",
            ),
            (
                "src/stream.rs",
                "use super::{CompressionAlgo, Encoder};\npub struct EncodeStream;\n",
            ),
            (
                "src/handler.rs",
                "use crate::FlowCtrl;\npub trait Handler { fn handle(&self, c: &mut FlowCtrl); }\n",
            ),
            (
                "src/routing.rs",
                "use crate::Handler;\npub struct FlowCtrl { handlers: Vec<Box<dyn Handler>> }\n",
            ),
        ];
        let g = graph_from_sources(root, sources);
        let p = |f: &str| root.join(f);
        assert_eq!(
            kinds(&g, &p("src/encoder.rs"), &p("src/lib.rs")),
            [EdgeKind::Enclosing, EdgeKind::Enclosing]
        );
        assert_eq!(
            internal_uses(&g, &p("src/lib.rs")),
            [p("src/encoder.rs"), p("src/stream.rs")],
            "the root's use of its children is a dependency"
        );
        assert_eq!(
            internal_uses(&g, &p("src/stream.rs")),
            [p("src/encoder.rs")],
            "a sibling's item, reached through super, is"
        );
        assert_eq!(
            g.cycles(),
            vec![vec![p("src/handler.rs"), p("src/routing.rs")]]
        );
    }

    /// This repository's own shape, both of its facades included:
    ///   * the crate root declares its modules and re-exports them —
    ///     `pub(crate) use editor::{codeview, viewer};`, a message type, and
    ///     whole modules by glob (`pub(crate) use app::model::*;`) — while
    ///     every `src/app/*.rs` does `use crate::*;` (and a sibling prelude
    ///     glob), and the backend names `crate::Message` / `crate::PreparedSeg`;
    ///   * `src/ui.rs` declares children and re-exports each by glob
    ///     (`pub(crate) use panes::*;`) while every child does `use super::*;`;
    ///
    /// plus test modules that `use super::*`. None of it is a cycle: a
    /// re-export is a facade, a glob of an enclosing module is the module
    /// tree's namespace, and names brought in by a glob re-export are
    /// followed to the file that defines them. The previous test left both
    /// facades out and passed by construction while the real tree reported
    /// cycles.
    #[test]
    fn this_repos_module_shape_has_no_cycles() {
        let root = Path::new("/clew");
        let sources: &[(&str, &str)] = &[
            (
                "src/main.rs",
                "mod app;\nmod backend;\nmod editor;\nmod shell;\nmod ui;\n\
                 pub(crate) use editor::{codeview, viewer};\n\
                 pub(crate) use backend::{server, updater};\n\
                 pub(crate) use app::message::Handoff;\n\
                 pub use app::message::Message;\n\
                 pub(crate) use app::model::*;\n\
                 pub use app::state::{App, WalkState};\n\
                 pub(crate) use app::tasks::*;\n\
                 use crate::viewer::MAX_FILE_BYTES;\n\
                 fn main() { shell::boot(); }\n\
                 #[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
            ),
            (
                "src/app.rs",
                "mod prelude;\npub(crate) mod model;\npub(crate) mod tasks;\n\
                 pub(crate) mod message;\npub(crate) mod state;\nmod session;\n",
            ),
            (
                "src/app/prelude.rs",
                "pub(crate) use crate::viewer::Viewer;\n\
                 pub(crate) use std::collections::HashMap;\n",
            ),
            (
                "src/app/model.rs",
                "use crate::app::prelude::*;\nuse crate::*;\n\
                 pub struct PreparedSeg;\npub enum AskPin { A }\n",
            ),
            (
                "src/app/message.rs",
                "use crate::app::prelude::*;\nuse crate::*;\n\
                 pub enum Message { Go(PreparedSeg) }\npub struct Handoff;\n\
                 #[cfg(test)]\nmod tests { use super::*; }\n",
            ),
            (
                "src/app/state.rs",
                "use crate::app::prelude::*;\nuse crate::*;\n\
                 pub struct App { pub seg: PreparedSeg }\npub struct WalkState;\n",
            ),
            (
                "src/app/tasks.rs",
                "use crate::app::prelude::*;\nuse crate::*;\n\
                 pub fn load_file() -> Message { Message::Go(PreparedSeg) }\n",
            ),
            (
                "src/app/session.rs",
                "use crate::app::prelude::*;\nuse crate::*;\n\
                 impl App { pub fn open(&self) -> Message { load_file() } }\n",
            ),
            ("src/backend.rs", "pub mod server;\npub mod updater;\n"),
            (
                "src/backend/server.rs",
                "use crate::Message;\nuse crate::PreparedSeg;\npub struct Server;\n",
            ),
            (
                "src/backend/updater.rs",
                "use crate::Message;\npub fn check() {}\n",
            ),
            ("src/editor.rs", "pub mod codeview;\npub mod viewer;\n"),
            (
                "src/editor/codeview.rs",
                "use crate::Message;\nuse crate::viewer::Viewer;\npub struct CodeView;\n",
            ),
            (
                "src/editor/viewer.rs",
                "use crate::PreparedSeg;\npub const MAX_FILE_BYTES: usize = 1;\n\
                 pub struct Viewer { pub seg: PreparedSeg }\n",
            ),
            (
                "src/shell.rs",
                "use crate::Message;\n\npub fn boot() {}\n\n#[cfg(test)]\nmod tests {\n    \
                 use super::*;\n\n    #[test]\n    fn boots() { boot(); }\n}\n",
            ),
            (
                "src/ui.rs",
                "use crate::codeview::CodeView;\nuse crate::{App, Message};\n\
                 mod panes;\npub(crate) use panes::*;\n\
                 mod sidebar;\npub(crate) use sidebar::*;\n",
            ),
            (
                "src/ui/panes.rs",
                "use super::*;\n\
                 pub fn code_pane(app: &App) -> Option<Message> { sidebar_row(); None }\n",
            ),
            (
                "src/ui/sidebar.rs",
                "use super::*;\nuse crate::codeview::CodeView;\npub fn sidebar_row() {}\n",
            ),
        ];
        let g = graph_from_sources(root, sources);
        assert_eq!(g.cycles(), Vec::<Vec<PathBuf>>::new());
        let p = |f: &str| root.join(f);

        // Facade 1: the crate root re-exports its modules, and each app file
        // globs the root back. Both edges stay in the graph, as what they are.
        assert_eq!(
            kinds(&g, &p("src/main.rs"), &p("src/app/model.rs")),
            [EdgeKind::Reexport]
        );
        assert_eq!(
            kinds(&g, &p("src/app/model.rs"), &p("src/main.rs")),
            [EdgeKind::Enclosing]
        );
        // A glob of a SIBLING is a dependency like any other import.
        assert_eq!(
            internal_uses(&g, &p("src/app/model.rs")),
            [p("src/app/prelude.rs")]
        );
        // Names are followed to where they live: `crate::Message` through the
        // root's explicit re-export, `crate::PreparedSeg` through its GLOB
        // re-export of `app::model` — not to main.rs.
        assert_eq!(
            internal_uses(&g, &p("src/backend/server.rs")),
            [p("src/app/message.rs"), p("src/app/model.rs")]
        );
        assert_eq!(
            internal_uses(&g, &p("src/editor/viewer.rs")),
            [p("src/app/model.rs")]
        );
        assert_eq!(
            internal_uses(&g, &p("src/editor/codeview.rs")),
            [p("src/app/message.rs"), p("src/editor/viewer.rs")]
        );
        // The root's own private `use` is a real dependency.
        assert_eq!(
            internal_uses(&g, &p("src/main.rs")),
            [p("src/editor/viewer.rs")]
        );

        // Facade 2: `ui.rs` declares each child and re-exports it by glob,
        // each child globs `super` back.
        assert_eq!(
            kinds(&g, &p("src/ui.rs"), &p("src/ui/panes.rs")),
            [EdgeKind::ModDecl, EdgeKind::Reexport]
        );
        assert_eq!(
            kinds(&g, &p("src/ui/panes.rs"), &p("src/ui.rs")),
            [EdgeKind::Enclosing]
        );
        assert_eq!(
            internal_uses(&g, &p("src/ui.rs")),
            [
                p("src/app/message.rs"),
                p("src/app/state.rs"),
                p("src/editor/codeview.rs")
            ]
        );
        assert!(internal_uses(&g, &p("src/ui/panes.rs")).is_empty());

        // Test modules' `use super::*` name their own file: no edge at all.
        assert!(
            g.imports(&p("src/app/message.rs"))
                .iter()
                .all(|e| e.target != Target::Internal(p("src/app/message.rs")))
        );
        assert_eq!(
            internal_uses(&g, &p("src/shell.rs")),
            [p("src/app/message.rs")]
        );
        // `use editor::…` names the local module, not a package `editor`.
        assert_eq!(g.external_packages(), ["std"]);
        // Ownership is still in the graph, as mod edges.
        assert_eq!(
            kinds(&g, &p("src/main.rs"), &p("src/shell.rs")),
            [EdgeKind::ModDecl]
        );

        // Without the symbol index a glob re-exported name stops at the root
        // that re-exports it, by elimination only: the edge is inferred, so
        // it points at the wrong file (the app must hand the resolver its
        // symbols to follow the name home) but no longer closes a loop with
        // the root's own `use crate::viewer::…` — which it used to, reported
        // as a cycle through main.rs.
        let bare = graph_with(root, sources, false);
        assert_eq!(
            kinds(&bare, &p("src/editor/viewer.rs"), &p("src/main.rs")),
            [EdgeKind::Inferred]
        );
        assert!(bare.cycles().is_empty(), "{:?}", bare.cycles());
    }

    /// A15: a Rust name found nowhere — no module file, no re-export, no
    /// item the symbol index lists — is inferred to be an item of the module
    /// it was reached through: drawn there, but not a dependency, so it cannot
    /// close a cycle. A `const` or `static` is an item like any other (the
    /// outline tags them): found, not guessed — of the enclosing module here,
    /// a sibling's in the second graph, where the cycle it closes is still
    /// reported.
    #[test]
    fn a_name_found_nowhere_is_inferred_and_closes_no_cycle() {
        let root = Path::new("/c");
        let g = graph_from_sources(
            root,
            &[
                (
                    "src/lib.rs",
                    "mod a;\nmod b;\nuse crate::a::run;\npub const LIMIT: usize = 1;\n\
                     pub static NAME: &str = \"n\";\n",
                ),
                ("src/a.rs", "use crate::Unknown;\npub fn run() {}\n"),
                ("src/b.rs", "use crate::LIMIT;\nuse crate::NAME;\n"),
            ],
        );
        let p = |f: &str| root.join(f);
        assert_eq!(
            kinds(&g, &p("src/a.rs"), &p("src/lib.rs")),
            [EdgeKind::Inferred]
        );
        assert_eq!(
            kinds(&g, &p("src/b.rs"), &p("src/lib.rs")),
            [EdgeKind::Enclosing, EdgeKind::Enclosing]
        );
        // lib.rs → a.rs (`use crate::a::run`) and a.rs → lib.rs (inferred):
        // no cycle. A real dependency back would be one.
        assert!(g.cycles().is_empty(), "{:?}", g.cycles());
        let g = graph_from_sources(
            root,
            &[
                ("src/lib.rs", "mod a;\nmod b;\n"),
                ("src/a.rs", "use crate::b::LIMIT;\npub fn run() {}\n"),
                (
                    "src/b.rs",
                    "use crate::a::run;\npub const LIMIT: usize = 1;\n",
                ),
            ],
        );
        assert_eq!(g.cycles(), [vec![p("src/a.rs"), p("src/b.rs")]]);
    }

    /// An inference stays one across re-export hops. `crate::Gen` is not in
    /// the crate (it comes in through `pub use ext::*` from an external
    /// crate): the chain lib → b (`pub use crate::Gen`) → lib runs out of
    /// hops and falls back to lib.rs — by elimination, which the edge has to
    /// say. Dropped on the way back, a.rs → lib.rs read as a dependency and
    /// closed a false cycle with lib.rs's `use a::A`.
    #[test]
    fn an_inference_survives_a_re_export_chain() {
        let root = Path::new("/c");
        let g = graph_from_sources(
            root,
            &[
                (
                    "src/lib.rs",
                    "mod a;\nmod b;\npub use ext::*;\npub use b::*;\nuse a::A;\n",
                ),
                ("src/b.rs", "pub use crate::Gen;\n"),
                ("src/a.rs", "use crate::Gen;\npub struct A;\n"),
            ],
        );
        let p = |f: &str| root.join(f);
        assert_eq!(
            kinds(&g, &p("src/a.rs"), &p("src/lib.rs")),
            [EdgeKind::Inferred]
        );
        assert!(g.cycles().is_empty(), "{:?}", g.cycles());
    }

    /// Input extraction did not scope and dropped the glob's star (an older
    /// server or cache): the test module's `use super::*` arrives as
    /// `super`, resolves to the parent — and still counts as the glob of an
    /// enclosing module it can only have been, so there is no cycle.
    #[test]
    fn unscoped_test_module_glob_is_still_not_a_cycle() {
        let root = Path::new("/clew");
        let r = resolver(root, &["src/main.rs", "src/shell.rs"]);
        let mut raw = HashMap::new();
        raw.insert(
            root.join("src/main.rs"),
            vec![RawImport {
                module: "shell".into(),
                line: 1,
                is_mod_decl: true,
            }],
        );
        raw.insert(root.join("src/shell.rs"), vec![ri("super")]);
        let g = ImportGraph::build(raw, &r, lang_of);
        let (main, shell) = (root.join("src/main.rs"), root.join("src/shell.rs"));
        assert_eq!(kinds(&g, &shell, &main), [EdgeKind::Enclosing]);
        assert!(internal_uses(&g, &shell).is_empty());
        assert!(g.cycles().is_empty());
    }

    /// A genuine dependency cycle between modules is still reported — also
    /// one made of globs, when what they glob are siblings, not ancestors.
    #[test]
    fn real_use_cycles_are_still_detected() {
        let root = Path::new("/p");
        let g = graph_from_sources(
            root,
            &[
                ("src/lib.rs", "mod a;\nmod b;\nmod c;\nmod d;\n"),
                ("src/a.rs", "use crate::b::B;\npub struct A;\n"),
                ("src/b.rs", "use crate::a::A;\npub struct B;\n"),
                ("src/c.rs", "use crate::d::*;\npub struct C;\n"),
                ("src/d.rs", "use super::c::*;\npub struct D;\n"),
            ],
        );
        assert_eq!(
            g.cycles(),
            vec![
                vec![root.join("src/a.rs"), root.join("src/b.rs")],
                vec![root.join("src/c.rs"), root.join("src/d.rs")],
            ]
        );
    }

    /// A name reaches a glob re-export's file only when it is known to be
    /// there — a module file under it, an item it defines, or a name it
    /// re-exports in turn — and a name the importing module defines itself
    /// shadows every glob.
    #[test]
    fn glob_reexports_supply_only_names_known_to_be_there() {
        let root = Path::new("/w");
        let g = graph_from_sources(
            root,
            &[
                (
                    "src/main.rs",
                    "mod editor;\nmod model;\nmod tasks;\n\
                     pub(crate) use editor::*;\npub(crate) use model::*;\n\
                     pub(crate) use tasks::*;\npub struct Local;\n",
                ),
                ("src/editor.rs", "pub mod codeview;\n"),
                ("src/editor/codeview.rs", "pub struct CodeView;\n"),
                (
                    "src/model.rs",
                    "mod inner;\npub use inner::Deep;\npub struct Seg;\n",
                ),
                ("src/model/inner.rs", "pub struct Deep;\n"),
                (
                    "src/tasks.rs",
                    "use crate::model::Private;\npub fn run() {}\n",
                ),
                (
                    "src/user.rs",
                    "use crate::codeview::CodeView;\nuse crate::Seg;\nuse crate::run;\n\
                     use crate::Deep;\nuse crate::Local;\nuse crate::Private;\n\
                     use crate::Unknown;\n",
                ),
            ],
        );
        let p = |f: &str| root.join(f);
        let targets: Vec<(String, Target)> = g
            .imports(&p("src/user.rs"))
            .iter()
            .map(|e| (e.specifier.clone(), e.target.clone()))
            .collect();
        assert_eq!(
            targets,
            [
                // A module file under a glob-re-exported module.
                (
                    "crate::codeview::CodeView".into(),
                    Target::Internal(p("src/editor/codeview.rs"))
                ),
                // Items the glob targets define.
                ("crate::Seg".into(), Target::Internal(p("src/model.rs"))),
                ("crate::run".into(), Target::Internal(p("src/tasks.rs"))),
                // A name the glob target re-exports in turn.
                (
                    "crate::Deep".into(),
                    Target::Internal(p("src/model/inner.rs"))
                ),
                // The root's own item, and names nothing is known to supply,
                // stay with the root — a private import of a glob target is
                // not re-exported by the glob.
                ("crate::Local".into(), Target::Internal(p("src/main.rs"))),
                ("crate::Private".into(), Target::Internal(p("src/main.rs"))),
                ("crate::Unknown".into(), Target::Internal(p("src/main.rs"))),
            ]
        );
        // The root's own item is the enclosing module's — no dependency of
        // one file on another; the names nothing is known to supply are only
        // inferred to be the root's.
        let kind_of = |spec: &str| {
            g.imports(&p("src/user.rs"))
                .iter()
                .find(|e| e.specifier == spec)
                .map(|e| e.kind)
        };
        assert_eq!(kind_of("crate::Local"), Some(EdgeKind::Enclosing));
        assert_eq!(kind_of("crate::Private"), Some(EdgeKind::Inferred));
        assert_eq!(kind_of("crate::Unknown"), Some(EdgeKind::Inferred));
    }

    /// The symbol index can change what a glob re-export names while no
    /// import changed at all (an item moved between files): a batch carrying
    /// only the two files' new items re-resolves the file that uses the item,
    /// whose own imports never changed.
    #[test]
    fn different_items_re_resolve_the_graph() {
        let root = Path::new("/w");
        let names = ["src/main.rs", "src/a.rs", "src/b.rs", "src/user.rs"];
        let r = resolver(root, &names);
        let decl = |m: &str| RawImport {
            module: m.into(),
            line: 1,
            is_mod_decl: true,
        };
        let item = |name: &str| RustItems::from([name_key(name)]);
        let file = |raw: Vec<RawImport>, items: RustItems| FileImports { raw, items };
        let mut batch = ImportBatch::default();
        batch.set(
            root.join("src/main.rs"),
            file(
                vec![decl("a"), decl("b"), ri("pub a::*"), ri("pub b::*")],
                RustItems::new(),
            ),
        );
        batch.set(root.join("src/a.rs"), file(vec![], item("Item")));
        batch.set(root.join("src/b.rs"), file(vec![], RustItems::new()));
        batch.set(
            root.join("src/user.rs"),
            file(vec![ri("crate::Item")], RustItems::new()),
        );
        let mut g = ImportGraph::default();
        g.apply(&batch, &r, lang_of);
        let target = |g: &ImportGraph| g.imports(&root.join("src/user.rs"))[0].target.clone();
        assert_eq!(target(&g), Target::Internal(root.join("src/a.rs")));
        // `Item` moved to b.rs; user.rs's own imports did not change.
        let mut moved = ImportBatch::default();
        moved.set(root.join("src/a.rs"), file(vec![], RustItems::new()));
        moved.set(root.join("src/b.rs"), file(vec![], item("Item")));
        assert!(g.apply(&moved, &r, lang_of).changed);
        assert_eq!(target(&g), Target::Internal(root.join("src/b.rs")));
        // The same items again: nothing to do, and nothing resolved.
        assert_eq!(
            g.apply(&moved, &r, lang_of),
            Applied {
                changed: false,
                structure_changed: false,
                scope_changed: false,
                resolved: 0,
                unresolvable: Vec::new(),
            }
        );
    }

    /// A1 / D1-4: what a batch costs follows what it can have changed, not
    /// the size of the project. Installing a whole project resolves each file
    /// ONCE (per-file installs re-resolved the whole graph for every Rust
    /// file: quadratic, on the UI thread, for up to 20,000 files); a batch of
    /// pages resolves only them; a Rust edit below the imports, or to a `use`
    /// nobody resolves through, resolves only its file; and a change to what
    /// other files DO resolve through — a module's items behind a glob
    /// re-export, the crate root's re-exports, a module removed — resolves
    /// exactly the files that read it; a new resolver resolves everything.
    /// After every batch the graph — its edges, and its reverse index — is
    /// what a from-scratch build of the same files gives.
    #[test]
    fn a_batch_resolves_in_proportion_to_what_it_can_have_changed() {
        let root = Path::new("/big");
        let (n, users) = (2_000, 20);
        let rust = |i: usize| format!("src/m{i}.rs");
        let user = |i: usize| format!("src/u{i}.rs");
        let js = |i: usize| format!("web/p{i}.js");
        let mut names: Vec<String> = vec!["src/lib.rs".into()];
        names.extend((0..n).map(rust));
        names.extend((0..users).map(user));
        names.extend((0..n).map(js));
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let r = resolver(root, &refs);
        let at = |line: usize, module: &str, is_mod_decl: bool| RawImport {
            module: module.into(),
            line,
            is_mod_decl,
        };
        let items = |names: &[&str]| -> RustItems { names.iter().map(|n| name_key(n)).collect() };
        // The crate root declares every module and re-exports m0's items by
        // glob; module i uses module i-1's item by its path; each user names
        // `crate::Item0`, which only the glob can supply; page i imports page
        // i-1.
        let lib = |extra: &[&str]| FileImports {
            raw: (0..n + users)
                .map(|i| match i < n {
                    true => at(i + 1, &format!("m{i}"), true),
                    false => at(i + 1, &format!("u{}", i - n), true),
                })
                .chain(extra.iter().map(|spec| at(n + users + 1, spec, false)))
                .collect(),
            items: RustItems::new(),
        };
        let rust_file = |i: usize, line: usize| FileImports {
            raw: match i {
                0 => vec![],
                _ => vec![at(
                    line,
                    &format!("crate::m{}::Item{}", i - 1, i - 1),
                    false,
                )],
            },
            items: items(&[&format!("Item{i}")]),
        };
        let user_file = FileImports {
            raw: vec![at(1, "crate::Item0", false)],
            items: RustItems::new(),
        };
        let js_file = |spec: &str| FileImports {
            raw: vec![at(1, spec, false)],
            items: RustItems::new(),
        };
        // Every file's current imports, for the from-scratch comparison.
        let mut now: HashMap<PathBuf, FileImports> = HashMap::new();
        let mut all = ImportBatch::default();
        let put = |now: &mut HashMap<PathBuf, FileImports>,
                   batch: &mut ImportBatch,
                   file: String,
                   imports: FileImports| {
            now.insert(root.join(&file), imports.clone());
            batch.set(root.join(file), imports);
        };
        put(&mut now, &mut all, "src/lib.rs".into(), lib(&["pub m0::*"]));
        for i in 0..n {
            put(&mut now, &mut all, rust(i), rust_file(i, 1));
            let spec = if i == 0 {
                "react".into()
            } else {
                format!("./p{}", i - 1)
            };
            put(&mut now, &mut all, js(i), js_file(&spec));
        }
        for i in 0..users {
            put(&mut now, &mut all, user(i), user_file.clone());
        }
        let mut g = ImportGraph::default();
        let applied = g.apply(&all, &r, lang_of);
        assert!(applied.changed);
        assert_eq!(
            applied.resolved,
            2 * n + users + 1,
            "each file resolves once"
        );
        let deps = |g: &ImportGraph, f: &str| internal_uses(g, &root.join(f));
        assert_eq!(deps(&g, "src/m7.rs"), [root.join("src/m6.rs")]);
        assert_eq!(deps(&g, "src/u3.rs"), [root.join("src/m0.rs")]);
        assert_eq!(deps(&g, "web/p7.js"), [root.join("web/p6.js")]);
        // Out-edges and the reverse index both, for every file there is.
        let assert_fresh =
            |g: &ImportGraph, now: &HashMap<PathBuf, FileImports>, r: &Resolver, step: &str| {
                let mut batch = ImportBatch::default();
                for (file, imports) in now {
                    batch.set(file.clone(), imports.clone());
                }
                let mut fresh = ImportGraph::default();
                fresh.apply(&batch, r, lang_of);
                for f in r.files.iter() {
                    assert_eq!(g.imports(f), fresh.imports(f), "{step}: {f:?}'s edges");
                    let (mut ours, mut theirs) = (g.importers(f), fresh.importers(f));
                    ours.sort();
                    theirs.sort();
                    assert_eq!(ours, theirs, "{step}: {f:?}'s importers");
                }
            };
        assert_fresh(&g, &now, &r, "installed");

        // Ten pages now import the first one: exactly those ten resolve.
        let mut pages = ImportBatch::default();
        for i in 100..110 {
            put(&mut now, &mut pages, js(i), js_file("./p0"));
        }
        let applied = g.apply(&pages, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, 10));
        assert!(applied.structure_changed, "ten new edges");
        assert!(applied.scope_changed, "ten pages import a project file now");
        assert_eq!(deps(&g, "web/p105.js"), [root.join("web/p0.js")]);
        assert_eq!(g.fan_in(&root.join("web/p0.js")), 11);
        assert_fresh(&g, &now, &r, "pages");

        // The crate root — which every user reads — gained a line above its
        // re-export (same declarations, same re-exports): its own edges
        // move, and its readers do not resolve.
        let mut shifted = ImportBatch::default();
        let mut moved = lib(&["pub m0::*"]);
        moved.raw.last_mut().unwrap().line += 1;
        put(&mut now, &mut shifted, "src/lib.rs".into(), moved);
        let applied = g.apply(&shifted, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, 1));
        assert!(
            !applied.structure_changed && !applied.scope_changed,
            "lines alone moved: no cycle, ranking or scope can have changed"
        );
        assert_fresh(&g, &now, &r, "shifted");

        // A module's `use` changed, and nobody resolves through it: its own
        // file resolves — not every Rust file, as it used to.
        let mut used = ImportBatch::default();
        let m300 = FileImports {
            raw: vec![at(1, "crate::m0::Item0", false)],
            items: items(&["Item300"]),
        };
        put(&mut now, &mut used, rust(300), m300);
        let applied = g.apply(&used, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, 1));
        assert_eq!(deps(&g, "src/m300.rs"), [root.join("src/m0.rs")]);
        assert_fresh(&g, &now, &r, "used");

        // m0 renames the item the glob gives the users: exactly the files
        // that read m0's items through the glob resolve — and now fall back
        // to the root, by elimination. m1, which names m0 by path, does not.
        let mut renamed = ImportBatch::default();
        let m0 = FileImports {
            raw: vec![],
            items: items(&["Item0b"]),
        };
        put(&mut now, &mut renamed, rust(0), m0);
        let applied = g.apply(&renamed, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, users));
        assert_eq!(
            kinds(&g, &root.join("src/u3.rs"), &root.join("src/lib.rs")),
            [EdgeKind::Inferred]
        );
        assert_eq!(deps(&g, "src/m1.rs"), [root.join("src/m0.rs")]);
        assert_fresh(&g, &now, &r, "renamed");

        // The root re-exports another module's items too: the users (who
        // read the root's re-exports) and the root itself resolve — and the
        // users find `Item0` again, in m7.
        let mut reexported = ImportBatch::default();
        put(
            &mut now,
            &mut reexported,
            "src/lib.rs".into(),
            lib(&["pub m0::*", "pub m7::*"]),
        );
        let mut m7 = rust_file(7, 1);
        m7.items.insert(name_key("Item0"));
        put(&mut now, &mut reexported, rust(7), m7);
        let applied = g.apply(&reexported, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, users + 1));
        assert_eq!(deps(&g, "src/u3.rs"), [root.join("src/m7.rs")]);
        assert_fresh(&g, &now, &r, "reexported");

        // A page deleted: nothing resolves, the graph still changes — its
        // out-edges and its share of the reverse index are gone.
        assert_eq!(g.fan_in(&root.join(js(499))), 1);
        let mut gone = ImportBatch::default();
        gone.remove(root.join(js(500)));
        now.remove(&root.join(js(500)));
        let applied = g.apply(&gone, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, 0));
        assert!(g.imports(&root.join(js(500))).is_empty());
        assert_eq!(g.fan_in(&root.join(js(499))), 0);
        assert_fresh(&g, &now, &r, "page deleted");

        // m7 is removed, and the users read its items (through the root's
        // glob): exactly they resolve, and find `Item0` nowhere now.
        let mut removed = ImportBatch::default();
        removed.remove(root.join(rust(7)));
        now.remove(&root.join(rust(7)));
        let applied = g.apply(&removed, &r, lang_of);
        assert_eq!((applied.changed, applied.resolved), (true, users));
        assert_eq!(
            kinds(&g, &root.join("src/u3.rs"), &root.join("src/lib.rs")),
            [EdgeKind::Inferred]
        );
        assert_fresh(&g, &now, &r, "module removed");

        // A new resolver (the file set changed): everything resolves against
        // it, and m8's `use crate::m7::Item7` no longer finds the file.
        let fewer: Vec<&str> = refs.iter().copied().filter(|f| *f != "src/m7.rs").collect();
        let r2 = resolver(root, &fewer);
        let mut again = ImportBatch::default();
        again.reresolve();
        let applied = g.apply(&again, &r2, lang_of);
        assert_eq!(applied.resolved, now.len());
        assert!(deps(&g, "src/m8.rs").is_empty());
        assert_fresh(&g, &now, &r2, "new resolver");
    }

    /// The scope the project call graph links through — the project files
    /// each file imports — moves only when one of those sets does. A package
    /// import, an unresolved one, a second import of a file already imported
    /// or lines moving change the edges (the first three their structure) and
    /// leave it be; so do removing a file that imports no project file and a
    /// reset that restates the same imports. A project file imported, or a
    /// file that imported one removed, moves it.
    #[test]
    fn the_scope_moves_only_with_the_project_files_imported() {
        let root = Path::new("/s");
        let r = resolver(root, &["a.py", "b.py", "c.py"]);
        let at = |line: usize, module: &str| RawImport {
            module: module.into(),
            line,
            is_mod_decl: false,
        };
        let file = |raw: &[RawImport]| FileImports {
            raw: raw.to_vec(),
            items: RustItems::new(),
        };
        let batch = |files: &[(&str, &[RawImport])]| {
            let mut batch = ImportBatch::default();
            for (f, raw) in files {
                batch.set(root.join(f), file(raw));
            }
            batch
        };
        let mut g = ImportGraph::default();
        let applied = g.apply(
            &batch(&[("a.py", &[at(1, "b")]), ("b.py", &[]), ("c.py", &[])]),
            &r,
            lang_of,
        );
        assert!(applied.scope_changed, "a imports b");

        let a = [at(1, "b"), at(2, "os"), at(3, ".gone")];
        let applied = g.apply(&batch(&[("a.py", &a)]), &r, lang_of);
        assert!(applied.structure_changed, "two edges gained");
        assert!(
            matches!(
                g.imports(&root.join("a.py"))[2].target,
                Target::Unresolved(_)
            ),
            "precondition: `.gone` resolves nowhere"
        );
        assert!(!applied.scope_changed, "neither is a project file");

        let a = [at(2, "b"), at(3, "os"), at(4, ".gone")];
        let applied = g.apply(&batch(&[("a.py", &a)]), &r, lang_of);
        assert!(applied.changed && !applied.structure_changed && !applied.scope_changed);

        let a = [at(2, "b"), at(3, "os"), at(4, ".gone"), at(5, "b")];
        let applied = g.apply(&batch(&[("a.py", &a)]), &r, lang_of);
        assert!(applied.structure_changed, "an edge gained");
        assert!(!applied.scope_changed, "b was imported already");

        let applied = g.apply(&batch(&[("c.py", &[at(1, "b")])]), &r, lang_of);
        assert!(applied.scope_changed, "c imports b");

        let mut restated = batch(&[]);
        restated.reset();
        for (f, raw) in [("a.py", &a[..]), ("b.py", &[]), ("c.py", &[at(1, "b")])] {
            restated.set(root.join(f), file(raw));
        }
        let applied = g.apply(&restated, &r, lang_of);
        assert!(applied.changed && applied.structure_changed);
        assert!(!applied.scope_changed, "the same imports, restated");

        let mut gone = batch(&[]);
        gone.remove(root.join("b.py"));
        let applied = g.apply(&gone, &r, lang_of);
        assert!(applied.structure_changed, "b left the graph");
        assert!(!applied.scope_changed, "b imported no project file");

        let mut gone = batch(&[]);
        gone.remove(root.join("c.py"));
        assert!(g.apply(&gone, &r, lang_of).scope_changed, "c imported b");
    }

    /// A remote publication reports a file it has nothing for as an empty
    /// entry: a deletion when the file set no longer lists it, else a file
    /// that has no imports (decided against the resolver's file set).
    #[test]
    fn an_empty_entry_is_a_deletion_only_for_an_unlisted_file() {
        let root = Path::new("/r");
        let r = resolver(root, &["a.py", "b.py"]);
        let mut batch = ImportBatch::default();
        batch.set(
            root.join("a.py"),
            FileImports {
                raw: vec![ri("b")],
                items: RustItems::new(),
            },
        );
        batch.set(
            root.join("gone.py"),
            FileImports {
                raw: vec![ri("b")],
                items: RustItems::new(),
            },
        );
        let mut g = ImportGraph::default();
        g.apply(&batch, &r, lang_of);
        assert_eq!(
            g.importers(&root.join("b.py")),
            [root.join("a.py"), root.join("gone.py")]
        );
        let mut emptied = ImportBatch::default();
        emptied.set_if_listed(root.join("a.py"), FileImports::default());
        emptied.set_if_listed(root.join("gone.py"), FileImports::default());
        assert!(g.apply(&emptied, &r, lang_of).changed);
        assert!(g.importers(&root.join("b.py")).is_empty());
        let files = g.files();
        assert!(
            files.contains(&root.join("a.py")),
            "a listed file stays, with no imports"
        );
        assert!(
            !files.contains(&root.join("gone.py")),
            "an unlisted one is gone"
        );
    }

    /// The initial index is an older read than any change the watcher queued
    /// meanwhile, so it never replaces one — and a reset drops every change
    /// queued before it.
    #[test]
    fn a_queued_change_is_replaced_only_by_a_newer_one() {
        let (a, b) = (PathBuf::from("/q/a.py"), PathBuf::from("/q/b.py"));
        let newer = FileImports {
            raw: vec![ri("b")],
            items: RustItems::new(),
        };
        let mut batch = ImportBatch::default();
        batch.set(a.clone(), newer.clone());
        batch.set_unless_queued(a.clone(), FileImports::default());
        batch.set_unless_queued(b.clone(), FileImports::default());
        assert_eq!(batch.files[&a], FileChange::Set(newer.clone().into()));
        assert_eq!(
            batch.files[&b],
            FileChange::Set(FileImports::default().into())
        );
        batch.remove(b.clone());
        assert_eq!(batch.files[&b], FileChange::Removed);
        batch.reset();
        assert!(batch.files.is_empty() && batch.reset && batch.needs_new_resolver());
    }

    /// A failed job's batch runs once more AHEAD of what was queued since: it
    /// keeps its changes to the files nothing touched since, and the
    /// re-resolve or reset it carried; a later change to one of its files
    /// wins (applied after it). A reset queued since restates the project
    /// without it: nothing of it is left to run.
    #[test]
    fn a_failed_batch_is_retried_ahead_of_the_later_changes() {
        let [a, b, c] = ["a", "b", "c"].map(|f| PathBuf::from(format!("/q/{f}.py")));
        let imports = |module: &str| FileImports {
            raw: vec![ri(module)],
            items: RustItems::new(),
        };
        let mut failed = ImportBatch::default();
        failed.set(a.clone(), imports("older"));
        failed.set(b.clone(), imports("b"));
        failed.reresolve();
        let mut later = ImportBatch::default();
        later.set(a.clone(), imports("newer"));
        later.remove(c.clone());
        assert!(failed.retry_before(&later));
        assert_eq!(
            failed.files.keys().collect::<Vec<_>>(),
            [&b],
            "a's later change wins"
        );
        assert_eq!(failed.files[&b], FileChange::Set(imports("b").into()));
        assert!(failed.needs_new_resolver() && !failed.reset);

        // Only a change the later batch makes again is dropped: a batch
        // whose every file changed since, with nothing else to redo, is done.
        let mut superseded = ImportBatch::default();
        superseded.set(a.clone(), imports("older"));
        assert!(!superseded.retry_before(&later));
        let mut failed_reset = ImportBatch::default();
        failed_reset.reset();
        failed_reset.set(a.clone(), imports("older"));
        assert!(failed_reset.retry_before(&later), "the reset is still owed");
        assert!(failed_reset.reset && failed_reset.files.is_empty());

        let mut restated = ImportBatch::default();
        restated.reset();
        restated.set(c.clone(), imports("c"));
        let mut failed = ImportBatch::default();
        failed.set(b.clone(), imports("b"));
        assert!(!failed.retry_before(&restated), "superseded by the reset");
    }

    /// A job applies its batch where the window keeps it: the imports it
    /// installs are the batch's own, shared, not copies (a job used to copy
    /// the whole batch first). And a batch that restates the project starts
    /// from an empty graph and compares its scope with the window's graph
    /// itself: no working copy of the graph it replaces is made, and kept
    /// alive to the end of the job, for that.
    #[test]
    fn a_job_shares_its_batch_and_copies_no_graph_it_restates() {
        let root = Path::new("/s");
        let r = resolver(root, &["a.py", "b.py"]);
        let a = root.join("a.py");
        let restating = |reset: bool| {
            let mut batch = ImportBatch::default();
            if reset {
                batch.reset();
            }
            for (file, raw) in [("a.py", vec![ri("b")]), ("b.py", Vec::new())] {
                let items = RustItems::new();
                batch.set(root.join(file), FileImports { raw, items });
            }
            batch
        };
        let batch = restating(false);
        let (graph, applied) = ImportGraph::applied_to(Arc::default(), &batch, &r, lang_of);
        assert!(applied.scope_changed);
        let FileChange::Set(queued) = &batch.files[&a] else {
            unreachable!()
        };
        assert!(
            Arc::ptr_eq(&graph.raw[&a], &queued.raw),
            "the job installed a copy of the batch"
        );

        // The window's graph, restated as it is: judged against it, and never
        // copied — a copy would share each file's edges with it.
        let window = Arc::new(graph);
        let edges = window.out[&a].clone();
        let sharing = std::cell::Cell::new(0);
        let lang = |p: &Path| {
            sharing.set(sharing.get().max(Arc::strong_count(&edges)));
            lang_of(p)
        };
        let (restated, applied) =
            ImportGraph::applied_to(window.clone(), &restating(true), &r, lang);
        assert!(
            applied.changed && !applied.scope_changed,
            "the same, restated"
        );
        assert_eq!(restated.importers(&root.join("b.py")), [a]);
        assert_eq!(sharing.get(), 2, "a copy of the restated graph was made");
    }

    #[test]
    fn rust_paths_follow_reexports_and_local_modules() {
        let root = Path::new("/w");
        let files = [
            "src/lib.rs",
            "src/editor.rs",
            "src/editor/codeview.rs",
            "src/app.rs",
            "src/app/message.rs",
            "src/ui.rs",
            "src/ui/panes.rs",
            "src/lsp/mod.rs",
            "src/lsp/client.rs",
            "src/a/b/c.rs",
            "src/a/x.rs",
            "src/a/nested/b.rs",
        ];
        let r = resolver(root, &files);
        let mut raw: HashMap<PathBuf, Vec<RawImport>> = HashMap::new();
        let decl = |m: &str| RawImport {
            module: m.into(),
            line: 1,
            is_mod_decl: true,
        };
        raw.insert(
            root.join("src/lib.rs"),
            vec![
                decl("editor"),
                decl("app"),
                decl("ui"),
                decl("lsp"),
                ri("editor::codeview"),
                ri("app::message::Message"),
                ri("clew_core::docs"),
            ],
        );
        let ctx = RustCtx::new(&raw, &lang_of);
        let res = |spec: &str, from: &str| {
            r.resolve_with(
                &ri(spec),
                &root.join(from),
                "rust",
                &ctx,
                &Declared::default(),
            )
        };
        let internal = |f: &str| Target::Internal(root.join(f));
        // A module re-exported by the crate root, reached through `crate::`.
        assert_eq!(
            res("crate::codeview::CodeView", "src/ui/panes.rs"),
            internal("src/editor/codeview.rs")
        );
        // A re-exported item is followed to its module.
        assert_eq!(
            res("crate::Message", "src/ui.rs"),
            internal("src/app/message.rs")
        );
        // An extern crate's module re-exported by the root is external.
        assert_eq!(
            res("crate::docs::DocEntry", "src/ui.rs"),
            Target::External("clew_core".into())
        );
        // No module and no re-export: an item of the crate root.
        assert_eq!(res("crate::Config", "src/ui.rs"), internal("src/lib.rs"));
        // 2018 paths: a declared local module first, else an extern crate.
        assert_eq!(
            res("editor::codeview", "src/lib.rs"),
            internal("src/editor/codeview.rs")
        );
        assert_eq!(
            res("serde::Deserialize", "src/lib.rs"),
            Target::External("serde".into())
        );
        // An uppercase head is a local item in scope, never a crate.
        assert_eq!(res("Kind::*", "src/ui.rs"), internal("src/ui.rs"));
        // `super` reaches the parent MODULE FILE, not only a crate root.
        assert_eq!(res("super", "src/ui/panes.rs"), internal("src/ui.rs"));
        assert_eq!(
            res("super::Thing", "src/lsp/client.rs"),
            internal("src/lsp/mod.rs")
        );
        // Every `super` climbs one module.
        assert_eq!(
            res("super::super::x::f", "src/a/b/c.rs"),
            internal("src/a/x.rs")
        );
        // A `mod b;` written inside inline `mod nested { … }`.
        let nested = RawImport {
            module: "nested::b".into(),
            line: 1,
            is_mod_decl: true,
        };
        assert_eq!(
            r.resolve_with(
                &nested,
                &root.join("src/a.rs"),
                "rust",
                &ctx,
                &Declared::default()
            ),
            internal("src/a/nested/b.rs")
        );
    }

    // --- JS / TS ---

    /// A workspace's packages import one another by name: `@acme/api` is
    /// the file its `package.json` exports — its source, before what a
    /// build makes of it, and a `dist/` file read as the `src/` one it was
    /// built from — a subpath by its own export or a `*` pattern, an
    /// unexported one by its path in `src/`; a package of the project's
    /// with none of those files is still the package; any other name, a
    /// package from npm. A Vue or Svelte file's imports resolve as a
    /// TypeScript file's.
    #[test]
    fn workspace_packages_resolve_by_name() {
        let root = Path::new("/p");
        let package = |name: &str, exports: &[(&str, &[&str])]| clew_protocol::JsPackage {
            name: name.into(),
            exports: exports
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|f| f.to_string()).collect()))
                .collect(),
        };
        let r = resolver(
            root,
            &[
                "apps/web/src/page.tsx",
                "apps/web/src/Card.vue",
                "packages/api/src/index.ts",
                "packages/api/src/client.ts",
                "packages/api/src/routers/post.ts",
                "packages/ui/src/index.ts",
                "packages/ui/src/button.tsx",
                "packages/db/index.ts",
            ],
        )
        .with_manifests(&clew_protocol::SourceManifests {
            js: vec![
                (
                    "packages/api/package.json".into(),
                    package(
                        "@acme/api",
                        &[
                            (".", &["./dist/index.d.ts"]),
                            ("./client", &["./src/client.ts"]),
                            ("./routers/*", &["./src/routers/*.ts"]),
                        ],
                    ),
                ),
                ("packages/ui/package.json".into(), package("@acme/ui", &[])),
                ("packages/db/package.json".into(), package("db", &[])),
                (
                    "packages/gone/package.json".into(),
                    package("@acme/gone", &[]),
                ),
            ],
            ..Default::default()
        });
        let from = root.join("apps/web/src/page.tsx");
        let res = |s: &str| r.resolve(&ri(s), &from, "tsx");
        let at = |rel: &str| Target::Internal(root.join(rel));
        assert_eq!(
            res("@acme/api"),
            at("packages/api/src/index.ts"),
            "dist read as src"
        );
        assert_eq!(res("@acme/api/client"), at("packages/api/src/client.ts"));
        assert_eq!(
            res("@acme/api/routers/post"),
            at("packages/api/src/routers/post.ts")
        );
        assert_eq!(
            res("@acme/ui"),
            at("packages/ui/src/index.ts"),
            "no exports: src/index"
        );
        assert_eq!(res("@acme/ui/button"), at("packages/ui/src/button.tsx"));
        assert_eq!(res("db"), at("packages/db/index.ts"), "its folder's index");
        assert_eq!(res("@acme/gone"), Target::External("@acme/gone".into()));
        assert_eq!(res("react"), Target::External("react".into()));
        assert_eq!(
            res("@tanstack/react-query/devtools"),
            Target::External("@tanstack/react-query".into())
        );
        // A Vue file's script imports as TypeScript's do.
        let vue = root.join("apps/web/src/Card.vue");
        assert_eq!(
            r.resolve(&ri("@acme/ui"), &vue, "vue"),
            at("packages/ui/src/index.ts")
        );
        assert_eq!(
            r.resolve(&ri("./page"), &vue, "vue"),
            at("apps/web/src/page.tsx")
        );
        assert_eq!(
            r.resolve(&ri("./Card.vue"), &from, "tsx"),
            at("apps/web/src/Card.vue"),
            "a component file by its own name"
        );
    }

    #[test]
    fn js_emitted_extensions_map_to_their_typescript_sources() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "src/app.ts",
                "src/util.ts",
                "src/view.tsx",
                "src/m.mts",
                "src/plain.js",
                "src/types.d.ts",
            ],
        );
        let from = root.join("src/app.ts");
        let res = |s: &str| r.resolve(&ri(s), &from, "typescript");
        assert_eq!(res("./util.js"), Target::Internal(root.join("src/util.ts")));
        assert_eq!(
            res("./view.jsx"),
            Target::Internal(root.join("src/view.tsx"))
        );
        assert_eq!(
            res("./view.js"),
            Target::Internal(root.join("src/view.tsx"))
        );
        assert_eq!(res("./m.mjs"), Target::Internal(root.join("src/m.mts")));
        assert_eq!(
            res("./types.js"),
            Target::Internal(root.join("src/types.d.ts"))
        );
        // A real .js file wins as written.
        assert_eq!(
            res("./plain.js"),
            Target::Internal(root.join("src/plain.js"))
        );
        assert_eq!(
            res("./missing.js"),
            Target::Unresolved("./missing.js".into())
        );
        // CommonJS `require('./util')` and dynamic `import('./view')` carry the
        // same specifiers and resolve the same way.
        assert_eq!(res("./util"), Target::Internal(root.join("src/util.ts")));
        assert_eq!(res("node:fs"), Target::External("node".into()));
    }

    #[test]
    fn remote_configs_are_fetched_with_their_bases_and_match_a_local_read() {
        let root = Path::new("/p");
        let files: Vec<PathBuf> = [
            "tsconfig.json",
            "tsconfig.base.json",
            "src/app.ts",
            "src/components/Button.tsx",
            "packages/web/tsconfig.json",
            "packages/web/app/main.ts",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let remote: HashMap<&str, &str> = HashMap::from([
            ("tsconfig.json", r#"{ "extends": "./tsconfig.base.json", }"#),
            (
                "tsconfig.base.json",
                r#"{ "extends": "../outside.json",
                     "compilerOptions": { "baseUrl": "./src", "paths": { "@/*": ["./*"] } } }"#,
            ),
            (
                "packages/web/tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "~/*": ["./app/*"] } } }"#,
            ),
        ]);
        let asked = std::cell::RefCell::new(Vec::<Vec<String>>::new());
        // The host: what it holds is read, and what it does not is missing.
        let fetch = |rels: Vec<String>| {
            asked.borrow_mut().push(rels.clone());
            let (held, missing): (Vec<String>, Vec<String>) = rels
                .into_iter()
                .partition(|r| remote.contains_key(r.as_str()));
            std::future::ready(crate::sources::HostSources {
                files: held
                    .into_iter()
                    .map(|r| {
                        let text = remote[r.as_str()].to_string();
                        (r, text)
                    })
                    .collect(),
                missing,
                ..Default::default()
            })
        };
        let configs =
            iced::futures::executor::block_on(load_remote_ts_configs(root, &files, fetch))
                .expect("the configs load");
        // The configs first (shallowest first), then the base the root
        // config extends — and never the `../outside.json` beyond the
        // project root.
        assert_eq!(
            *asked.borrow(),
            [
                vec![
                    "tsconfig.json".to_string(),
                    "packages/web/tsconfig.json".to_string()
                ],
                vec!["tsconfig.base.json".to_string()],
            ]
        );
        // Exactly what reading the same files locally yields.
        let local = |p: &Path| {
            p.strip_prefix(root)
                .ok()
                .and_then(|r| remote.get(r.to_str()?))
                .map(|t| t.to_string())
        };
        assert_eq!(configs, ts_configs_from(root, &files, &local));
        let r = resolver(
            root,
            &files
                .iter()
                .map(|f| f.strip_prefix(root).unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
        )
        .with_ts_configs(configs);
        assert_eq!(
            r.resolve(
                &ri("@/components/Button"),
                &root.join("src/app.ts"),
                "typescript"
            ),
            Target::Internal(root.join("src/components/Button.tsx"))
        );

        // A server that cannot be asked at all is an error, not "no aliases".
        let down = |rels: Vec<String>| {
            std::future::ready(crate::sources::HostSources {
                unread: rels.into_iter().map(|r| (r, Some("down".into()))).collect(),
                ..Default::default()
            })
        };
        assert_eq!(
            iced::futures::executor::block_on(load_remote_ts_configs(root, &files, down)),
            Err("down".to_string())
        );
        // A project with no configs asks for nothing.
        let none = |_: Vec<String>| -> std::future::Ready<crate::sources::HostSources> {
            panic!("nothing to fetch")
        };
        let plain = vec![root.join("src/app.ts")];
        assert_eq!(
            iced::futures::executor::block_on(load_remote_ts_configs(root, &plain, none)),
            Ok(TsConfigs::default())
        );
    }

    /// What a remote host says of each config and base it is asked for: one
    /// not there is gone; one too large, no plain text file or not readable
    /// is not read, and named in the note as what it is — the host's cap or
    /// this side's, not UTF-8, the host's error — while the other configs
    /// apply; and one it did not answer for fails the fetch, at any depth of
    /// a chain. Such a base was taken for one not there, and its aliases
    /// resolved as packages; one the host could not read failed the fetch,
    /// and with it every alias of the project, for good.
    #[test]
    fn remote_configs_not_sent_are_named_and_one_not_read_fails_the_fetch() {
        let root = Path::new("/p");
        let files: Vec<PathBuf> = [
            "tsconfig.json",
            "base.json",
            "a/tsconfig.json",
            "b/tsconfig.json",
            "c/tsconfig.json",
            "d/tsconfig.json",
            "e/tsconfig.json",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let base = r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"] } } }"#;
        let host = |base_read: bool| {
            move |rels: Vec<String>| {
                let mut got = crate::sources::HostSources::default();
                for r in rels {
                    match r.as_str() {
                        "tsconfig.json" => got
                            .files
                            .push((r, r#"{ "extends": "./base.json" }"#.into())),
                        "base.json" if base_read => got.files.push((r, base.into())),
                        "base.json" => got.unread.push((r, None)),
                        "a/tsconfig.json" => got.too_large.push((r, 600 * 1024)),
                        "b/tsconfig.json" | "e/tsconfig.json" => {
                            got.refused.push((r, clew_protocol::Refusal::NotUtf8))
                        }
                        "c/tsconfig.json" => got
                            .unreadable
                            .push((r, "Permission denied (os error 13)".into())),
                        "d/tsconfig.json" => {
                            let text = " ".repeat(MAX_TS_CONFIG_BYTES as usize + 1);
                            got.files.push((r, text));
                        }
                        _ => got.missing.push(r),
                    }
                }
                std::future::ready(got)
            }
        };
        let configs =
            iced::futures::executor::block_on(load_remote_ts_configs(root, &files, host(true)))
                .expect("the configs load");
        assert_eq!(configs.len(), 1, "{configs:?}");
        assert_eq!(configs[0].base_url.as_deref(), Some(root));
        assert_eq!(
            configs.cap_note().as_deref(),
            Some(
                "tsconfig/jsconfig: a/tsconfig.json (too large, 600 KiB), b/tsconfig.json (not \
                 UTF-8), c/tsconfig.json (could not be read: Permission denied (os error 13)) \
                 and 2 more not read"
            )
        );
        assert_eq!(
            iced::futures::executor::block_on(load_remote_ts_configs(root, &files, host(false))),
            Err("base.json could not be read there".to_string())
        );
    }

    #[test]
    fn tsconfig_paths_and_base_url() {
        let root = Path::new("/p");
        let files = [
            "tsconfig.json",
            "tsconfig.base.json",
            "src/app.ts",
            "src/components/Button.tsx",
            "src/config/index.ts",
            "libs/core/src/index.ts",
            "packages/web/tsconfig.json",
            "packages/web/app/main.ts",
            "packages/web/app/lib/api.ts",
        ];
        let texts: HashMap<PathBuf, &str> = HashMap::from([
            (
                root.join("tsconfig.base.json"),
                r#"{
                    // Shared settings.
                    "compilerOptions": {
                        "baseUrl": "./src",
                        "paths": {
                            "@/*": ["./*"],            /* relative to baseUrl */
                            "@core": ["../libs/core/src/index.ts"],
                            "config": ["./config"],
                        },
                    },
                }"#,
            ),
            (
                root.join("tsconfig.json"),
                r#"{ "extends": "./tsconfig.base.json", }"#,
            ),
            (
                root.join("packages/web/tsconfig.json"),
                r#"{ "compilerOptions": { "paths": { "~/*": ["./app/*"] } } }"#,
            ),
        ]);
        let read = |p: &Path| texts.get(p).map(|s| s.to_string());
        let configs: Vec<TsConfig> = ["tsconfig.json", "packages/web/tsconfig.json"]
            .iter()
            .filter_map(|c| load_ts_config(root, &root.join(c), &read, 0))
            .collect();
        assert_eq!(
            configs.len(),
            2,
            "JSONC with comments and trailing commas parses"
        );
        let r = resolver(root, &files).with_ts_configs(TsConfigs::from(configs));
        let app = root.join("src/app.ts");
        let res = |s: &str, from: &Path| r.resolve(&ri(s), from, "typescript");
        let internal = |f: &str| Target::Internal(root.join(f));
        assert_eq!(
            res("@/components/Button", &app),
            internal("src/components/Button.tsx")
        );
        assert_eq!(res("@core", &app), internal("libs/core/src/index.ts"));
        assert_eq!(res("config", &app), internal("src/config/index.ts"));
        // baseUrl makes bare specifiers project-relative first…
        assert_eq!(
            res("components/Button", &app),
            internal("src/components/Button.tsx")
        );
        // …and anything else stays a package.
        assert_eq!(res("react", &app), Target::External("react".into()));
        assert_eq!(
            res("@scope/pkg/x", &app),
            Target::External("@scope/pkg".into())
        );
        // An alias whose target is missing falls through, as in TypeScript;
        // `@/…` can never be a package name, so it stays unresolved.
        assert_eq!(res("@/nope", &app), Target::Unresolved("@/nope".into()));
        // The nearest config applies: the package's own `~/*`.
        let web = root.join("packages/web/app/main.ts");
        assert_eq!(
            res("~/lib/api", &web),
            internal("packages/web/app/lib/api.ts")
        );
    }

    /// Configs parsed from in-memory texts (`rel → text`) under `root`.
    fn configs_of(root: &Path, texts: &[(&str, &str)]) -> TsConfigs {
        let texts: HashMap<PathBuf, String> = texts
            .iter()
            .map(|(rel, text)| (root.join(rel), text.to_string()))
            .collect();
        let files: Vec<PathBuf> = texts.keys().cloned().collect();
        ts_configs_from(root, &files, &|p: &Path| texts.get(p).cloned())
    }

    /// A solution config — Vite's `tsconfig.json`, of `references` and no
    /// aliases — takes those of the config it references that has them:
    /// `@/x` is the app's `src/x`.
    #[test]
    fn a_solution_config_takes_its_apps_aliases() {
        let root = Path::new("/p");
        let configs = configs_of(
            root,
            &[
                (
                    "tsconfig.json",
                    r#"{ "files": [], "references": [{ "path": "./tsconfig.node.json" }, { "path": "./tsconfig.app.json" }] }"#,
                ),
                ("tsconfig.node.json", r#"{ "include": ["vite.config.ts"] }"#),
                (
                    "tsconfig.app.json",
                    r#"{ "compilerOptions": { "paths": { "~/*": ["./src/*"] } } }"#,
                ),
            ],
        );
        let r =
            resolver(root, &["src/main.ts", "src/lib/utils.ts"]).with_ts_configs(configs.clone());
        assert_eq!(
            r.resolve(&ri("~/lib/utils"), &root.join("src/main.ts"), "typescript"),
            Target::Internal(root.join("src/lib/utils.ts"))
        );
        assert!(
            configs.reads(&root.join("tsconfig.app.json")),
            "an edit to it re-resolves"
        );
    }

    /// TypeScript's merge: `paths` resolve against the FINAL `baseUrl`,
    /// wherever in the `extends` chain it was set, else against the
    /// directory of the config that declared them.
    #[test]
    fn tsconfig_paths_resolve_against_the_merged_base_url() {
        let root = Path::new("/p");
        let files = [
            "src/x/widget.ts",
            "src/shared/util.ts",
            "lib/core.ts",
            "app/main.ts",
            "web/src/a.ts",
            "web/main.ts",
        ];
        // The child declares `paths`; the base it extends sets `baseUrl`.
        let own_paths = configs_of(
            root,
            &[
                (
                    "base.json",
                    r#"{ "compilerOptions": { "baseUrl": "./src" } }"#,
                ),
                (
                    "tsconfig.json",
                    r#"{ "extends": "./base.json",
                         "compilerOptions": { "paths": { "@x/*": ["x/*"] } } }"#,
                ),
            ],
        );
        let r = resolver(root, &files).with_ts_configs(own_paths);
        assert_eq!(
            r.resolve(&ri("@x/widget"), &root.join("app/main.ts"), "typescript"),
            Target::Internal(root.join("src/x/widget.ts")),
            "own paths, inherited baseUrl"
        );
        // The base declares `paths` (in its own directory); the child sets
        // `baseUrl` — the inherited paths follow the child's baseUrl.
        let inherited_paths = configs_of(
            root,
            &[
                (
                    "configs/base.json",
                    r#"{ "compilerOptions": { "paths": { "@s/*": ["shared/*"] } } }"#,
                ),
                (
                    "tsconfig.json",
                    r#"{ "extends": "./configs/base.json",
                         "compilerOptions": { "baseUrl": "./src" } }"#,
                ),
            ],
        );
        let r = resolver(root, &files).with_ts_configs(inherited_paths);
        assert_eq!(
            r.resolve(&ri("@s/util"), &root.join("app/main.ts"), "typescript"),
            Target::Internal(root.join("src/shared/util.ts")),
            "inherited paths, own baseUrl"
        );
        // No baseUrl anywhere: the declaring config's directory.
        let no_base = configs_of(
            root,
            &[
                (
                    "configs/base.json",
                    r#"{ "compilerOptions": { "paths": { "@c/*": ["../lib/*"] } } }"#,
                ),
                ("tsconfig.json", r#"{ "extends": "./configs/base.json" }"#),
            ],
        );
        assert_eq!(no_base[0].paths_base, root.join("configs"));
        let r = resolver(root, &files).with_ts_configs(no_base);
        assert_eq!(
            r.resolve(&ri("@c/core"), &root.join("app/main.ts"), "typescript"),
            Target::Internal(root.join("lib/core.ts")),
        );
        // An inherited baseUrl stays relative to the config that set it.
        let nested = configs_of(
            root,
            &[
                (
                    "web/base.json",
                    r#"{ "compilerOptions": { "baseUrl": "src" } }"#,
                ),
                ("web/tsconfig.json", r#"{ "extends": "./base.json" }"#),
            ],
        );
        assert_eq!(nested[0].base_url, Some(root.join("web/src")));
        let r = resolver(root, &files).with_ts_configs(nested);
        assert_eq!(
            r.resolve(&ri("a"), &root.join("web/main.ts"), "typescript"),
            Target::Internal(root.join("web/src/a.ts")),
        );
    }

    /// A directory holding both configs is configured by its tsconfig —
    /// TypeScript ignores the jsconfig there — even though `jsconfig.json`
    /// sorts first.
    #[test]
    fn a_directory_with_both_configs_uses_its_tsconfig() {
        let root = Path::new("/p");
        let configs = configs_of(
            root,
            &[
                (
                    "jsconfig.json",
                    r#"{ "compilerOptions": { "paths": { "@/*": ["js/*"] } } }"#,
                ),
                (
                    "tsconfig.json",
                    r#"{ "compilerOptions": { "paths": { "@/*": ["ts/*"] } } }"#,
                ),
            ],
        );
        assert_eq!(configs.len(), 1);
        assert_eq!(
            configs[0].paths,
            [("@/*".to_string(), vec!["ts/*".to_string()])]
        );
    }

    /// A hostile repository's `extends` chains cost what their files do, not
    /// what the paths through them do: a base naming itself a hundred times,
    /// and a diamond (two bases extending one, which a second config extends
    /// too), have each file read and parsed once per resolve and listed once
    /// among the configs' sources. Every entry used to be followed afresh: the
    /// self-reference alone cost a million parses, and recorded as many paths.
    #[test]
    fn an_extends_chain_reads_and_lists_each_file_once() {
        let root = Path::new("/p");
        let reads = std::cell::RefCell::new(HashMap::<PathBuf, usize>::new());
        let load = |texts: &[(&str, &str)]| {
            reads.borrow_mut().clear();
            let texts: HashMap<PathBuf, String> = texts
                .iter()
                .map(|(rel, text)| (root.join(rel), text.to_string()))
                .collect();
            let files: Vec<PathBuf> = texts.keys().cloned().collect();
            let read = |p: &Path| {
                *reads.borrow_mut().entry(p.to_path_buf()).or_default() += 1;
                texts.get(p).cloned()
            };
            ts_configs_from(root, &files, &read)
        };
        let at = |rels: &[&str]| {
            rels.iter()
                .map(|rel| root.join(rel))
                .collect::<HashSet<_>>()
        };

        let selfish = format!(
            r#"{{ "extends": [{}], "compilerOptions": {{ "baseUrl": "./src" }} }}"#,
            [r#""./base.json""#; 100].join(", ")
        );
        let configs = load(&[
            ("tsconfig.json", r#"{ "extends": "./base.json" }"#),
            ("base.json", &selfish),
        ]);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs.sources, at(&["tsconfig.json", "base.json"]));
        assert_eq!(
            configs[0].base_url,
            Some(root.join("src")),
            "the base applies"
        );
        assert_eq!(
            *reads.borrow(),
            HashMap::from([(root.join("tsconfig.json"), 1), (root.join("base.json"), 1)])
        );

        let configs = load(&[
            (
                "tsconfig.json",
                r#"{ "extends": ["./a.json", "./b.json"] }"#,
            ),
            (
                "a.json",
                r#"{ "extends": "./c.json", "compilerOptions": { "paths": { "@a/*": ["a/*"] } } }"#,
            ),
            ("b.json", r#"{ "extends": "./c.json" }"#),
            ("c.json", r#"{ "compilerOptions": { "baseUrl": "./src" } }"#),
            ("web/tsconfig.json", r#"{ "extends": "../c.json" }"#),
        ]);
        // Deepest first: the web config, then the root one.
        assert_eq!(configs[0].dir, root.join("web"));
        assert_eq!(configs[0].base_url, Some(root.join("src")));
        assert_eq!(
            configs.sources,
            at(&[
                "web/tsconfig.json",
                "tsconfig.json",
                "a.json",
                "b.json",
                "c.json"
            ])
        );
        assert_eq!(
            configs[1].paths,
            [("@a/*".to_string(), vec!["a/*".to_string()])]
        );
        assert_eq!(configs[1].paths_base, root.join("src"));
        let reads = reads.borrow();
        assert_eq!(reads.len(), 5);
        assert!(reads.values().all(|&n| n == 1), "{reads:?}");
    }

    /// One `extends` array is followed to its first `MAX_TS_EXTENDS_ENTRIES`
    /// entries, in file order; the rest are ignored as though the config did
    /// not name them — not read, not merged, not listed. Every entry used to
    /// be followed and listed, however many a 1 MiB file could hold.
    #[test]
    fn an_extends_array_is_followed_to_its_cap() {
        let root = Path::new("/p");
        let named = MAX_TS_EXTENDS_ENTRIES + 10;
        let mut texts: HashMap<PathBuf, String> = (0..named)
            .map(|i| {
                let text = format!(r#"{{ "compilerOptions": {{ "baseUrl": "./src{i}" }} }}"#);
                (root.join(format!("b{i}.json")), text)
            })
            .collect();
        let bases: Vec<String> = (0..named).map(|i| format!(r#""./b{i}.json""#)).collect();
        texts.insert(
            root.join("tsconfig.json"),
            format!(r#"{{ "extends": [{}] }}"#, bases.join(", ")),
        );
        let reads = std::cell::Cell::new(0usize);
        let read = |p: &Path| {
            reads.set(reads.get() + 1);
            texts.get(p).cloned()
        };
        let configs = ts_configs_from(root, &[root.join("tsconfig.json")], &read);
        let last = MAX_TS_EXTENDS_ENTRIES - 1;
        assert_eq!(
            configs[0].base_url,
            Some(root.join(format!("src{last}"))),
            "the last base followed is the one in effect"
        );
        assert_eq!(reads.get(), 1 + MAX_TS_EXTENDS_ENTRIES);
        assert_eq!(configs.sources.len(), 1 + MAX_TS_EXTENDS_ENTRIES);
        assert!(configs.reads(&root.join(format!("b{last}.json"))));
        assert!(
            !configs.reads(&root.join(format!("b{}.json", last + 1))),
            "an entry past the cap is listed"
        );
        assert_eq!(
            configs.cap_note().as_deref(),
            Some("tsconfig/jsconfig: 10 `extends` entries past the first 32 of an array ignored"),
            "the cap is silent"
        );
    }

    /// However the chains fan out, one resolve reads and lists at most
    /// `MAX_TS_SOURCES` files — the configs themselves first — and a base
    /// reached once that budget is spent is ignored wherever it is named:
    /// here two levels of bases, each as wide as an array may be, name more
    /// than the budget, and a second config's own base comes too late.
    #[test]
    fn the_files_one_resolve_reads_are_capped() {
        let root = Path::new("/p");
        let width = MAX_TS_EXTENDS_ENTRIES;
        let extends = |names: Vec<String>| {
            let names: Vec<String> = names.iter().map(|n| format!(r#""./{n}""#)).collect();
            format!(r#"{{ "extends": [{}] }}"#, names.join(", "))
        };
        let level = |i: usize| (0..width).map(|j| format!("l2_{i}_{j}.json")).collect();
        let mut texts: HashMap<PathBuf, String> = (0..width)
            .map(|i| (root.join(format!("l1_{i}.json")), extends(level(i))))
            .collect();
        texts.insert(
            root.join("tsconfig.json"),
            extends((0..width).map(|i| format!("l1_{i}.json")).collect()),
        );
        texts.insert(
            root.join("web/tsconfig.json"),
            r#"{ "extends": "./late.json" }"#.to_string(),
        );
        texts.insert(
            root.join("web/late.json"),
            r#"{ "compilerOptions": { "baseUrl": "." } }"#.to_string(),
        );
        assert!(
            1 + width + width * width > MAX_TS_SOURCES,
            "precondition: the chains name more than the budget"
        );
        let reads = std::cell::RefCell::new(HashMap::<PathBuf, usize>::new());
        let read = |p: &Path| {
            *reads.borrow_mut().entry(p.to_path_buf()).or_default() += 1;
            texts.get(p).cloned()
        };
        let files = [root.join("tsconfig.json"), root.join("web/tsconfig.json")];
        let configs = ts_configs_from(root, &files, &read);
        assert_eq!(configs.sources.len(), MAX_TS_SOURCES);
        let reads = reads.borrow();
        assert!(reads.len() <= MAX_TS_SOURCES && reads.values().all(|&n| n == 1));
        assert!(
            reads.keys().all(|p| configs.reads(p)),
            "a file read unlisted"
        );
        // The second config is one of them, but its base is past the budget.
        let web = configs.iter().find(|c| c.dir == root.join("web")).unwrap();
        assert!(configs.reads(&root.join("web/tsconfig.json")));
        assert!(!configs.reads(&root.join("web/late.json")));
        assert_eq!(web.base_url, None, "a base past the budget applied");
        let said = format!("more base configs not read (limit {MAX_TS_SOURCES} files)");
        assert!(
            configs.cap_note().is_some_and(|note| note.contains(&said)),
            "the budget is silent: {:?}",
            configs.cap_note()
        );
    }

    /// The shape that cost millions — one base naming 70,000 distinct files
    /// (within the 1 MiB a config may take), extended by each of the 64
    /// configs a project may have — reads and lists a hundred files, each
    /// once: the configs, the base, and the base's first
    /// `MAX_TS_EXTENDS_ENTRIES` entries. Every config used to list the whole
    /// chain (4.5 million paths, which each changed path was then compared
    /// with on the window's thread).
    #[test]
    fn a_huge_shared_base_costs_its_capped_entries_once() {
        let root = Path::new("/p");
        let entries: Vec<String> = (0..70_000).map(|i| format!(r#""./x{i}""#)).collect();
        let base = format!(r#"{{ "extends": [{}] }}"#, entries.join(","));
        assert!(base.len() as u64 <= MAX_TS_CONFIG_BYTES, "precondition");
        let mut texts = HashMap::from([(root.join("base.json"), base)]);
        let files: Vec<PathBuf> = (0..MAX_TS_CONFIGS)
            .map(|i| root.join(format!("p{i}/tsconfig.json")))
            .collect();
        for path in &files {
            texts.insert(path.clone(), r#"{ "extends": "../base.json" }"#.into());
        }
        let reads = std::cell::RefCell::new(HashMap::<PathBuf, usize>::new());
        let read = |p: &Path| {
            *reads.borrow_mut().entry(p.to_path_buf()).or_default() += 1;
            texts.get(p).cloned()
        };
        let configs = ts_configs_from(root, &files, &read);
        assert_eq!(configs.len(), MAX_TS_CONFIGS);
        let listed = MAX_TS_CONFIGS + 1 + MAX_TS_EXTENDS_ENTRIES;
        assert_eq!(configs.sources.len(), listed);
        let reads = reads.borrow();
        assert_eq!(reads.len(), listed);
        assert!(reads.values().all(|&n| n == 1), "a file read twice");
    }

    /// The cap keeps the SHALLOWEST configs: sorted by path, a monorepo's
    /// `packages/…` configs used the whole budget and dropped the root one.
    #[test]
    fn the_config_cap_keeps_the_root_config() {
        let root = Path::new("/p");
        let mut files: Vec<PathBuf> = (0..MAX_TS_CONFIGS + 10)
            .map(|i| root.join(format!("packages/p{i:03}/tsconfig.json")))
            .collect();
        files.push(root.join("tsconfig.json"));
        files.push(root.join("packages/p000/jsconfig.json"));
        let kept = ts_config_paths(&files);
        assert_eq!(kept.len(), MAX_TS_CONFIGS);
        assert_eq!(kept[0], &root.join("tsconfig.json"));
        assert!(
            kept.iter()
                .all(|p| !p.ends_with("packages/p000/jsconfig.json")),
            "one config per directory"
        );
        // The ten packages past the cap, and the root config's place.
        let configs = ts_configs_from(root, &files, &|_| None);
        assert_eq!(
            configs.cap_note().as_deref(),
            Some("tsconfig/jsconfig: 11 more configs not read (limit 64)"),
            "the cap is silent"
        );
    }

    /// A panic's message goes to the status line in short: its first line,
    /// cut at 80 characters; a payload that is not text says so.
    #[test]
    fn a_panic_is_noted_in_short() {
        let note = |payload: Box<dyn std::any::Any + Send>| panic_note(&*payload);
        assert_eq!(note(Box::new("index out of bounds")), "index out of bounds");
        let long = format!("{}\nat the second line", "x".repeat(100));
        assert_eq!(note(Box::new(long)), format!("{}…", "x".repeat(80)));
        assert_eq!(note(Box::new(7_u8)), "no message");
    }

    /// An alias whose local target is missing is not "broken": TypeScript
    /// goes on to `baseUrl` and then `node_modules`, so it can be the
    /// installed package of that name.
    #[test]
    fn an_alias_whose_target_is_missing_falls_through_to_packages() {
        let root = Path::new("/p");
        let configs = configs_of(
            root,
            &[(
                "tsconfig.json",
                r#"{ "compilerOptions": { "baseUrl": ".", "paths": {
                        "config": ["./settings/config"],
                        "@lib/*": ["./libs/*"],
                        "*": ["./types/*"] } } }"#,
            )],
        );
        let r = resolver(root, &["src/app.ts", "libs/ui.ts", "vendor/tool.ts"])
            .with_ts_configs(configs);
        let res = |s: &str| r.resolve(&ri(s), &root.join("src/app.ts"), "typescript");
        assert_eq!(res("config"), Target::External("config".into()));
        assert_eq!(res("@lib/ui"), Target::Internal(root.join("libs/ui.ts")));
        assert_eq!(res("@lib/gone"), Target::External("@lib/gone".into()));
        // The catch-all misses too, then `baseUrl` finds it.
        assert_eq!(
            res("vendor/tool"),
            Target::Internal(root.join("vendor/tool.ts"))
        );
    }

    /// A local config is read once and remembered until it changes; a
    /// changed one is read again, and one replaced by a symlink is refused
    /// like on a first read.
    #[test]
    fn local_config_reads_are_remembered_until_the_file_changes() {
        let dir = clew_core::testutil::TempDir::new("tsconfig-cache");
        let path = dir.join("tsconfig.json");
        std::fs::write(&path, r#"{ "compilerOptions": { "baseUrl": "a" } }"#).unwrap();
        let first = read_ts_config_text(&path).expect("a plain file reads");
        assert!(first.is_some());
        assert_eq!(read_ts_config_text(&path), Ok(first));
        std::fs::write(&path, r#"{ "compilerOptions": { "baseUrl": "a/b/c" } }"#).unwrap();
        assert!(
            read_ts_config_text(&path)
                .unwrap()
                .unwrap()
                .contains("a/b/c")
        );
        #[cfg(unix)]
        {
            let elsewhere = dir.join("elsewhere.json");
            std::fs::rename(&path, &elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
            assert_eq!(
                read_ts_config_text(&path),
                Err(TsNotRead::Refused(clew_protocol::Refusal::NotPlainFile))
            );
        }
        assert_eq!(read_ts_config_text(&dir.join("gone.json")), Ok(None));
    }

    /// A LOCAL config or base that is there and is not read — too large, not
    /// UTF-8, a link — is named in the note on what was left out, as what it
    /// is, as a remote project's is; the other configs apply. Each read as
    /// one not there, without a word, and its aliases resolved as packages.
    #[test]
    fn local_configs_not_read_are_named() {
        let dir = clew_core::testutil::TempDir::new("tsconfig-local-named");
        let root = dir.canonicalize().unwrap();
        let config =
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["./src/*"] } } }"#;
        std::fs::write(root.join("tsconfig.json"), config).unwrap();
        for sub in ["big", "latin1", "link"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let big = " ".repeat(MAX_TS_CONFIG_BYTES as usize + 1);
        std::fs::write(root.join("big/tsconfig.json"), &big).unwrap();
        std::fs::write(root.join("latin1/tsconfig.json"), b"{ \"caf\xe9\": 1 }").unwrap();
        let files: Vec<PathBuf> = ["tsconfig.json", "big/tsconfig.json", "latin1/tsconfig.json"]
            .iter()
            .map(|f| root.join(f))
            .collect();
        #[cfg(unix)]
        let files = {
            std::os::unix::fs::symlink(root.join("tsconfig.json"), root.join("link/tsconfig.json"))
                .unwrap();
            let mut files = files;
            files.push(root.join("link/tsconfig.json"));
            files
        };
        let configs = read_ts_configs(&root, &files);
        assert_eq!(configs.len(), 1, "the readable config applies");
        let note = configs.cap_note().expect("the configs not read are named");
        let mut want = format!(
            "tsconfig/jsconfig: big/tsconfig.json (too large, {} KiB), latin1/tsconfig.json (not \
             UTF-8)",
            (MAX_TS_CONFIG_BYTES + 1).div_ceil(1024)
        );
        if cfg!(unix) {
            want.push_str(", link/tsconfig.json (not a plain file)");
        }
        assert_eq!(note, format!("{want} not read"));
    }

    #[test]
    fn unconfigured_at_alias_means_src() {
        let root = Path::new("/p");
        let r = resolver(root, &["src/app.ts", "src/lib/api.ts"]);
        let from = root.join("src/app.ts");
        assert_eq!(
            r.resolve(&ri("@/lib/api"), &from, "typescript"),
            Target::Internal(root.join("src/lib/api.ts"))
        );
        assert_eq!(
            r.resolve(&ri("@/missing"), &from, "typescript"),
            Target::Unresolved("@/missing".into())
        );
    }

    #[test]
    fn strip_jsonc_keeps_strings_intact() {
        let text = "{\"a\": \"http://x/*y*/\", // c\n \"b\": [1, 2, /* x */ ],\n}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).expect("valid json");
        assert_eq!(v["a"], "http://x/*y*/");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }

    // --- Python ---

    /// `from .views import x` is recorded as `.views`, `from ..shared.util
    /// import x` as `..shared.util`: modules, reached as such.
    #[test]
    fn python_relative_from_imports_reach_the_module() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "pkg/__init__.py",
                "pkg/views.py",
                "pkg/shared/__init__.py",
                "pkg/shared/util.py",
                "pkg/sub/__init__.py",
                "pkg/sub/mod.py",
            ],
        );
        let res = |s: &str, from: &str| r.resolve(&ri(s), &root.join(from), "python");
        let internal = |f: &str| Target::Internal(root.join(f));
        assert_eq!(res(".views", "pkg/__init__.py"), internal("pkg/views.py"));
        assert_eq!(res("..views", "pkg/sub/mod.py"), internal("pkg/views.py"));
        assert_eq!(
            res("..shared.util", "pkg/sub/mod.py"),
            internal("pkg/shared/util.py")
        );
        // A bare `from . import …` of names in the package itself.
        assert_eq!(res(".", "pkg/sub/mod.py"), internal("pkg/sub/__init__.py"));
        assert_eq!(res("..", "pkg/sub/mod.py"), internal("pkg/__init__.py"));
        // `from .nope import x` names a module that is not there.
        assert_eq!(
            res(".nope", "pkg/__init__.py"),
            Target::Unresolved(".nope".into())
        );
    }

    /// `from shop import payments` reads `shop/payments.py`, not the
    /// package's `__init__.py`; `from shop.models import Tea` reads the
    /// module the name is defined in, and so does `from .models import Tea`
    /// (recorded `.models:Tea`). A namespace package (no `__init__.py`) is
    /// read through its submodules; another package's is external.
    #[test]
    fn python_from_import_names_a_submodule_before_the_package() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "shop/__init__.py",
                "shop/payments.py",
                "shop/models.py",
                "shop/checkout.py",
                "shop/sub/__init__.py",
                "spaced/thing.py",
                "main.py",
            ],
        );
        let res = |s: &str, from: &str| r.resolve(&ri(s), &root.join(from), "python");
        let internal = |f: &str| Target::Internal(root.join(f));
        assert_eq!(
            res("shop:payments", "main.py"),
            internal("shop/payments.py")
        );
        assert_eq!(
            res("shop:Inventory", "main.py"),
            internal("shop/__init__.py")
        );
        assert_eq!(
            res("shop.models:Tea", "main.py"),
            internal("shop/models.py")
        );
        assert_eq!(res("shop:sub", "main.py"), internal("shop/sub/__init__.py"));
        assert_eq!(res("spaced:thing", "main.py"), internal("spaced/thing.py"));
        assert_eq!(
            res("flask:Flask", "main.py"),
            Target::External("flask".into())
        );
        assert_eq!(
            res(".models:Tea", "shop/checkout.py"),
            internal("shop/models.py")
        );
        assert_eq!(
            res(".:payments", "shop/checkout.py"),
            internal("shop/payments.py")
        );
        assert_eq!(
            res(".nope:x", "shop/checkout.py"),
            Target::Unresolved(".nope".into())
        );
        // Straight from the extractor, end to end: each name its own file,
        // a star import the module's.
        let extracted = imports_of(
            "from shop import payments, Inventory\nfrom shop.models import Tea, Order\nfrom shop import *\n",
            "python",
        );
        let targets: Vec<Target> = extracted
            .iter()
            .map(|i| r.resolve(i, &root.join("main.py"), "python"))
            .collect();
        assert_eq!(
            targets,
            [
                internal("shop/payments.py"),
                internal("shop/__init__.py"),
                internal("shop/models.py"),
                internal("shop/models.py"),
                internal("shop/__init__.py"),
            ]
        );
    }

    /// `from . import name` (recorded `.:name`) names `name` OF the package:
    /// its submodule when there is one, else an attribute of the package —
    /// its `__init__.py`, which Python runs first either way. Only a name the
    /// package's own `__init__.py` imports from itself stays unresolved.
    #[test]
    fn python_from_dot_import_names_a_submodule_or_the_package() {
        let root = Path::new("/p");
        let r = resolver(
            root,
            &[
                "pkg/__init__.py",
                "pkg/views.py",
                "pkg/sub/__init__.py",
                "pkg/sub/mod.py",
                "loose/mod.py",
            ],
        );
        let res = |s: &str, from: &str| r.resolve(&ri(s), &root.join(from), "python");
        let internal = |f: &str| Target::Internal(root.join(f));
        // A submodule.
        assert_eq!(res(".:views", "pkg/__init__.py"), internal("pkg/views.py"));
        assert_eq!(res("..:views", "pkg/sub/mod.py"), internal("pkg/views.py"));
        // An attribute `__init__.py` defines (a function, `__version__`, a
        // name it imports): the package itself.
        assert_eq!(
            res(".:helper", "pkg/sub/mod.py"),
            internal("pkg/sub/__init__.py")
        );
        assert_eq!(
            res("..:VERSION", "pkg/sub/mod.py"),
            internal("pkg/__init__.py")
        );
        // Not from itself, and not from a directory that is no package.
        assert_eq!(
            res(".:helper", "pkg/__init__.py"),
            Target::Unresolved(".helper".into())
        );
        assert_eq!(
            res(".:thing", "loose/mod.py"),
            Target::Unresolved(".thing".into())
        );
        // Straight from the extractor, end to end.
        let extracted = imports_of("from . import views, helper\n", "python");
        let targets: Vec<Target> = extracted
            .iter()
            .map(|i| r.resolve(i, &root.join("pkg/__init__.py"), "python"))
            .collect();
        assert_eq!(
            targets,
            [
                internal("pkg/views.py"),
                Target::Unresolved(".helper".into())
            ]
        );
    }

    // --- Dart ---

    /// A pub workspace's packages import one another by name: the file under
    /// the named package's `lib/`, as its pubspec says it is named; a name
    /// no pubspec gives stays another's.
    #[test]
    fn dart_workspace_packages_import_one_another() {
        let root = Path::new("/w");
        let paths: Vec<PathBuf> = [
            "pkgs/shelf/pubspec.yaml",
            "pkgs/shelf/lib/shelf.dart",
            "pkgs/shelf/lib/src/request.dart",
            "pkgs/shelf_router/pubspec.yaml",
            "pkgs/shelf_router/lib/src/router.dart",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let r = Resolver::with_meta(root, &paths, None, None).with_manifests(
            &clew_protocol::SourceManifests {
                dart: vec![
                    ("pkgs/shelf/pubspec.yaml".into(), "shelf".into()),
                    (
                        "pkgs/shelf_router/pubspec.yaml".into(),
                        "shelf_router".into(),
                    ),
                ],
                ..Default::default()
            },
        );
        let res = |s: &str| {
            r.resolve(
                &ri(s),
                &root.join("pkgs/shelf_router/lib/src/router.dart"),
                "dart",
            )
        };
        assert_eq!(
            res("package:shelf/shelf.dart"),
            Target::Internal(root.join("pkgs/shelf/lib/shelf.dart"))
        );
        assert_eq!(
            res("package:shelf/src/request.dart"),
            Target::Internal(root.join("pkgs/shelf/lib/src/request.dart"))
        );
        assert_eq!(
            res("package:meta/meta.dart"),
            Target::External("package:meta".into())
        );
    }

    #[test]
    fn dart_package_relative_and_sdk_imports() {
        let root = Path::new("/d");
        let paths: Vec<PathBuf> = [
            "lib/app.dart",
            "lib/src/parser.dart",
            "lib/src/ast.dart",
            "bin/main.dart",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let r = Resolver::with_meta(root, &paths, None, Some("demo".into()));
        let res = |s: &str, from: &str| r.resolve(&ri(s), &root.join(from), "dart");
        let internal = |f: &str| Target::Internal(root.join(f));
        assert_eq!(
            res("package:demo/src/parser.dart", "bin/main.dart"),
            internal("lib/src/parser.dart")
        );
        assert_eq!(
            res("src/parser.dart", "lib/app.dart"),
            internal("lib/src/parser.dart")
        );
        assert_eq!(
            res("ast.dart", "lib/src/parser.dart"),
            internal("lib/src/ast.dart")
        );
        assert_eq!(
            res("../app.dart", "lib/src/parser.dart"),
            internal("lib/app.dart")
        );
        assert_eq!(
            res("dart:async", "lib/app.dart"),
            Target::External("dart:async".into())
        );
        assert_eq!(
            res("package:args/args.dart", "lib/app.dart"),
            Target::External("package:args".into())
        );
        // Our own package name with a file that does not exist.
        assert_eq!(
            res("package:demo/missing.dart", "lib/app.dart"),
            Target::External("package:demo".into())
        );
        assert_eq!(
            res("gone.dart", "lib/app.dart"),
            Target::Unresolved("gone.dart".into())
        );
    }

    /// Every target `file`'s edges point at, in source order.
    fn targets(g: &ImportGraph, file: &Path) -> Vec<Target> {
        g.imports(file).iter().map(|e| e.target.clone()).collect()
    }

    /// A Java import is the class file under the source root the importer's
    /// own `package` line implies — a file filed outside the Maven layout
    /// too — or under its module's other root (a test sees the main
    /// classes); a static member is found through its class; a star names
    /// the package's directory, which reaches every class of it. The
    /// `package` line is no edge, nor is a static import of the file's own
    /// class. A name in a package of the project's own that no file is stays
    /// unresolved; any other is its library's package.
    #[test]
    fn java_imports_resolve_under_the_packages_source_root() {
        let root = Path::new("/j");
        let main = "app/src/main/java/com/acme/app/Main.java";
        let strings = "app/src/main/java/com/acme/util/Strings.java";
        let user = "app/src/main/java/com/acme/model/User.java";
        let order = "app/src/main/java/com/acme/model/Order.java";
        let test = "app/src/test/java/com/acme/app/MainTest.java";
        let thing = "legacy/com/acme/old/Thing.java";
        let other = "legacy/com/acme/old2/Other.java";
        // A module's tests may use another's test fixtures (a test jar).
        let fixtures = "lib/src/test/java/com/acme/testing/Fixtures.java";
        let lib_test = "lib/src/test/java/com/acme/lib/LibTest.java";
        let app_it = "app/src/test/java/com/acme/app/AppIT.java";
        let g = graph_with(
            root,
            &[
                (
                    main,
                    "package com.acme.app;\n\nimport com.acme.util.Strings;\nimport com.acme.model.*;\n\
                     import static com.acme.util.Strings.join;\nimport java.util.List;\n\
                     import com.acme.util.Missing;\nimport static com.acme.app.Main.helper;\n\
                     class Main {}\n",
                ),
                (strings, "package com.acme.util;\npublic class Strings {}\n"),
                (user, "package com.acme.model;\npublic class User {}\n"),
                (order, "package com.acme.model;\npublic class Order {}\n"),
                (
                    test,
                    "package com.acme.app;\nimport com.acme.util.Strings;\nclass MainTest {}\n",
                ),
                (
                    thing,
                    "package com.acme.old;\nimport com.acme.old2.Other;\nclass Thing {}\n",
                ),
                (other, "package com.acme.old2;\npublic class Other {}\n"),
                (
                    fixtures,
                    "package com.acme.testing;\npublic class Fixtures {}\n",
                ),
                (lib_test, "package com.acme.lib;\nclass LibTest {}\n"),
                (
                    app_it,
                    "package com.acme.app;\nimport com.acme.testing.Fixtures;\nclass AppIT {}\n",
                ),
            ],
            false,
        );
        let at = |f: &str| root.join(f);
        let model = at("app/src/main/java/com/acme/model");
        assert_eq!(
            targets(&g, &at(main)),
            [
                Target::Internal(at(strings)),
                Target::Internal(model.clone()),
                Target::Internal(at(strings)),
                Target::External("java.util".into()),
                Target::Unresolved("com.acme.util.Missing".into()),
            ]
        );
        let mut reached = g.imported_files(&at(main), &g.packages());
        reached.sort();
        assert_eq!(reached, [at(order), at(user), at(strings)]);
        assert_eq!(g.importers(&at(user)), [at(main)]);
        assert_eq!(g.package_file(&model), Some(at(order)));
        assert_eq!(targets(&g, &at(test)), [Target::Internal(at(strings))]);
        assert_eq!(targets(&g, &at(thing)), [Target::Internal(at(other))]);
        assert_eq!(targets(&g, &at(app_it)), [Target::Internal(at(fixtures))]);
    }

    /// Kotlin and Scala resolve as Java does, across the three: a class is
    /// its file under its source root (`src/main/kotlin`, `src/main/scala`,
    /// or `src/main/java` beside Java), a star its package, and a Kotlin
    /// top-level function its package. An Elixir module is its file by
    /// Mix's convention under a `lib/` (an umbrella app's too); a library's
    /// module is its library's.
    #[test]
    fn kotlin_scala_and_elixir_imports_resolve() {
        let root = Path::new("/k");
        let app = "app/src/main/kotlin/com/acme/app/App.kt";
        let util = "app/src/main/kotlin/com/acme/util/Strings.kt";
        let format = "app/src/main/kotlin/com/acme/util/format.kt";
        let model = "app/src/main/java/com/acme/model/User.java";
        let order = "app/src/main/scala/com/acme/model/Order.scala";
        let shop = "shop/src/main/scala/com/acme/shop/Shop.scala";
        // A package in two roots: a test importing a helper `src/main`
        // defines, its package's folder under `src/test` too.
        let assert = "app/src/main/kotlin/com/acme/shared/Assert.kt";
        let fixtures = "app/src/test/kotlin/com/acme/shared/Fixtures.kt";
        let cache_test = "app/src/test/kotlin/com/acme/h2/CacheTest.kt";
        let g = graph_with(
            root,
            &[
                (assert, "package com.acme.shared\nfun assertSame() = 1\n"),
                (fixtures, "package com.acme.shared\nobject Fixtures\n"),
                (
                    cache_test,
                    "package com.acme.h2\n\nimport com.acme.shared.assertSame\n",
                ),
                (
                    app,
                    "package com.acme.app\n\nimport com.acme.util.Strings\nimport com.acme.util.formatPrice\n\
                     import com.acme.model.User\nimport com.acme.model.*\nimport kotlinx.coroutines.launch\n",
                ),
                (util, "package com.acme.util\nobject Strings\n"),
                (format, "package com.acme.util\nfun formatPrice() = 1\n"),
                (model, "package com.acme.model;\npublic class User {}\n"),
                (
                    order,
                    "package com.acme.model\ncase class Order(id: Long)\n",
                ),
                (
                    shop,
                    "package com.acme\npackage shop\n\nimport com.acme.model.{Order, User}\nimport scala.util.Try\n",
                ),
            ],
            false,
        );
        let at = |f: &str| root.join(f);
        assert_eq!(
            targets(&g, &at(app)),
            [
                Target::Internal(at(util)),
                Target::Internal(at("app/src/main/kotlin/com/acme/util")),
                Target::Internal(at(model)),
                // The package's Scala file is under another root.
                Target::Internal(at("package com.acme.model")),
                Target::External("kotlinx.coroutines".into()),
            ]
        );
        assert_eq!(
            targets(&g, &at(cache_test)),
            [Target::Internal(at("package com.acme.shared"))]
        );
        assert_eq!(
            targets(&g, &at(shop)),
            [
                Target::Internal(at(order)),
                Target::Internal(at(model)),
                Target::External("scala.util".into()),
            ]
        );

        let root = Path::new("/ex");
        let checkout = "lib/shop/checkout.ex";
        let tax = "lib/shop/pricing/tax.ex";
        let item = "apps/store/lib/store/order_item.ex";
        let client = "lib/shop/http_client.ex";
        let g = graph_with(
            root,
            &[
                (
                    checkout,
                    "defmodule Shop.Checkout do\n  alias Shop.Pricing.Tax\n  alias Store.{OrderItem}\n\
                     alias Shop.HTTPClient\n  use GenServer\n  import Ecto.Query\nend\n",
                ),
                (tax, "defmodule Shop.Pricing.Tax do\nend\n"),
                (item, "defmodule Store.OrderItem do\nend\n"),
                (client, "defmodule Shop.HTTPClient do\nend\n"),
            ],
            false,
        );
        assert_eq!(
            targets(&g, &root.join(checkout)),
            [
                Target::Internal(root.join(tax)),
                Target::Internal(root.join(item)),
                Target::Internal(root.join(client)),
                Target::External("GenServer".into()),
                Target::External("Ecto".into()),
            ]
        );
    }

    /// A C# `using` of a namespace points at the namespace's node, which
    /// reaches every file declaring it — wherever it is, block-bodied or
    /// file-scoped; a `using static` reaches its type's file. A namespace no
    /// project file declares is a library's. A file whose namespaces change
    /// moves the `using`s of them with it, as a build from scratch would
    /// have them.
    /// A Kotlin or Scala package is wherever its files say they are: an
    /// import of it, or of a class or member of it, that no folder of its
    /// name holds, reaches the files declaring it (`package os` under
    /// `os/src/`), as a C# `using` reaches its namespace's; a name no file
    /// declares stays a library's.
    #[test]
    fn jvm_imports_reach_the_files_declaring_their_package() {
        let root = Path::new("/jvm");
        let model = "os/src/Model.scala";
        let ops = "os/src/FileOps.scala";
        let tests = "os/test/src/PathTests.scala";
        let repo = "lib/data/Repo.kt";
        let service = "app/web/Service.kt";
        let sources = [
            (model, "package os\n\nclass Path(s: String)\n"),
            (
                ops,
                "package os\n\nobject read {\n  def apply(p: Path): String = \"\"\n}\n",
            ),
            (
                tests,
                "package test.os\n\nimport os._\nimport os.Path\nimport utest._\n\nobject PathTests\n",
            ),
            (repo, "package shop.data\n\nclass Repo\n"),
            (
                service,
                "package shop.web\n\nimport shop.data.Repo\nimport kotlinx.coroutines.launch\n\nclass Service(val repo: Repo)\n",
            ),
        ];
        let g = graph_with(root, &sources, false);
        let at = |f: &str| root.join(f);
        let names: Vec<&str> = sources.iter().map(|(f, _)| *f).collect();
        let r = resolver(root, &names);
        let os = r.package_node("os");
        assert_eq!(
            targets(&g, &at(tests)),
            [
                Target::Internal(os.clone()),
                Target::Internal(os.clone()),
                Target::External("utest".into()),
            ]
        );
        let mut reached = g.imported_files(&at(tests), &g.packages());
        reached.sort();
        assert_eq!(reached, [at(ops), at(model)]);
        assert_eq!(
            targets(&g, &at(service)),
            [
                Target::Internal(r.package_node("shop.data")),
                Target::External("kotlinx.coroutines".into()),
            ]
        );
        assert_eq!(g.imported_files(&at(service), &g.packages()), [at(repo)]);
    }

    #[test]
    fn csharp_usings_reach_the_files_declaring_their_namespace() {
        let root = Path::new("/cs");
        let user = "src/Core/User.cs";
        let order = "src/Model/Order.cs";
        let strings = "src/Util/Strings.cs";
        let program = "src/App/Program.cs";
        let sources = [
            (user, "namespace Acme.Core {\n  public class User {}\n}\n"),
            (order, "namespace Acme.Core;\npublic class Order {}\n"),
            (
                strings,
                "namespace Acme.Util { static class Strings {} static class Paths {} }\n",
            ),
            (
                program,
                "using Acme.Core;\nusing static Acme.Util.Strings;\nusing System.Text;\n\
                 using Json = Newtonsoft.Json;\nnamespace Acme.App { class P {} }\n",
            ),
        ];
        let mut g = graph_with(root, &sources, false);
        let at = |f: &str| root.join(f);
        let names: Vec<&str> = sources.iter().map(|(f, _)| *f).collect();
        let r = resolver(root, &names);
        let core = r.namespace_node("Acme.Core");
        assert_eq!(
            targets(&g, &at(program)),
            [
                Target::Internal(core.clone()),
                Target::Internal(at(strings)),
                Target::External("System.Text".into()),
                Target::External("Newtonsoft.Json".into()),
            ]
        );
        let mut reached = g.imported_files(&at(program), &g.packages());
        reached.sort();
        assert_eq!(reached, [at(user), at(order), at(strings)]);
        assert_eq!(g.importers(&at(order)), [at(program)]);
        assert_eq!(g.fan_in(&at(user)), 1);
        assert_eq!(g.package_file(&core), Some(at(user)));
        assert!(g.files().contains(&at(order)) && !g.files().contains(&core));

        // Both files leave the namespace: the `using` is no longer the
        // project's — re-resolved though Program.cs itself did not change.
        let mut batch = ImportBatch::default();
        for (file, src) in [
            (user, "namespace Acme.Domain { class User {} }\n"),
            (order, "namespace Acme.Domain;\nclass Order {}\n"),
        ] {
            let raw = crate::index::analyze_file(&at(file), file, src, "csharp").imports;
            batch.set(
                at(file),
                FileImports {
                    raw,
                    items: RustItems::new(),
                },
            );
        }
        let applied = g.apply(&batch, &r, lang_of);
        assert!(applied.scope_changed && applied.structure_changed);
        assert_eq!(
            targets(&g, &at(program))[0],
            Target::External("Acme.Core".into())
        );
        assert!(g.importers(&at(user)).is_empty());
        let scratch = graph_with(
            root,
            &[
                (user, "namespace Acme.Domain { class User {} }\n"),
                (order, "namespace Acme.Domain;\nclass Order {}\n"),
                sources[2],
                sources[3],
            ],
            false,
        );
        for file in [user, order, strings, program] {
            assert_eq!(g.imports(&at(file)), scratch.imports(&at(file)), "{file}");
        }
    }
}

#[cfg(test)]
mod web_resolution_tests {
    use super::*;

    /// A page's `/src/main.ts` from Vite's root, its stylesheet and a CDN
    /// script; a stylesheet's partials, index files, aliases, packages and
    /// Sass's built-ins.
    #[test]
    fn pages_and_stylesheets_resolve_as_browsers_and_sass_read_them() {
        let root = PathBuf::from("/p");
        let files: Vec<PathBuf> = [
            "web/index.html",
            "web/src/main.ts",
            "web/style.css",
            "web/public/main.css",
            "web/src/styles/main.scss",
            "web/src/styles/_variables.scss",
            "web/src/styles/mixins/_index.scss",
            "web/src/styles/theme.less",
            "web/src/styles/base.less",
            "src/styles/tokens.scss",
        ]
        .iter()
        .map(|f| root.join(f))
        .collect();
        let r = Resolver::new(&root, &files);
        let at = |from: &str, spec: &str, lang: &str| {
            let raw = RawImport {
                module: spec.to_string(),
                line: 1,
                is_mod_decl: false,
            };
            r.resolve(&raw, &root.join(from), lang)
        };
        let internal = |f: &str| Target::Internal(root.join(f));
        let page = "web/index.html";
        assert_eq!(
            at(page, "/src/main.ts", "html"),
            internal("web/src/main.ts")
        );
        assert_eq!(
            at(page, "./style.css?v=2", "html"),
            internal("web/style.css")
        );
        assert_eq!(
            at(page, "/main.css", "html"),
            internal("web/public/main.css")
        );
        assert_eq!(
            at(page, "https://cdn.example.com/x.js", "html"),
            Target::External("cdn.example.com".into())
        );
        let scss = "web/src/styles/main.scss";
        assert_eq!(
            at(scss, "variables", "scss"),
            internal("web/src/styles/_variables.scss")
        );
        assert_eq!(
            at(scss, "./mixins", "scss"),
            internal("web/src/styles/mixins/_index.scss")
        );
        assert_eq!(
            at(scss, "@/styles/tokens", "scss"),
            internal("src/styles/tokens.scss")
        );
        assert_eq!(
            at(scss, "sass:math", "scss"),
            Target::External("sass:math".into())
        );
        assert_eq!(
            at(scss, "bootstrap/scss/bootstrap", "scss"),
            Target::External("bootstrap".into())
        );
        assert_eq!(
            at(scss, "~bootstrap/scss/x", "scss"),
            Target::External("bootstrap".into())
        );
        let less = "web/src/styles/theme.less";
        assert_eq!(
            at(less, "base", "less"),
            internal("web/src/styles/base.less")
        );
        assert_eq!(
            at(less, "./missing", "less"),
            Target::Unresolved("./missing".into())
        );
    }
}
